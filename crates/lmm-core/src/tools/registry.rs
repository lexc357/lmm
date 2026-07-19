//! Built-in per-game catalogs of modding tools, configuration tweaks and
//! plugin-list locations — the data behind the Game Tools section.
//!
//! Everything here is a snapshot of each game's community-standard modding
//! setup. Adding a tool = adding a `ToolDef` row; adding a game = adding a
//! `GameTools` entry. Nothing in this module touches the filesystem.

use serde::Serialize;

/// What a tool is, for display and for special handling (script extenders
/// are launched through the game, not directly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolKind {
    /// Loader that must start instead of (or inject into) the game binary.
    ScriptExtender,
    /// A plugin/runtime dependency for script-extender mods (e.g. Address
    /// Library); installs like a mod but is really infrastructure.
    ExtenderPlugin,
    /// Load-order sorter (LOOT and friends).
    LoadOrderTool,
    /// Plugin/record editor (the xEdit family).
    Editor,
    /// Asset builder or patch generator (BodySlide, Pandora, DynDOLOD).
    Builder,
    /// Mod-loading framework (SMAPI, RED4ext, ...).
    Framework,
    /// Anything else.
    Utility,
}

impl ToolKind {
    pub fn describe(self) -> &'static str {
        match self {
            ToolKind::ScriptExtender => "script extender",
            ToolKind::ExtenderPlugin => "extender plugin",
            ToolKind::LoadOrderTool => "load-order tool",
            ToolKind::Editor => "editor",
            ToolKind::Builder => "builder",
            ToolKind::Framework => "framework",
            ToolKind::Utility => "utility",
        }
    }
}

/// How strongly the game's modding community expects this tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    /// Practically every modded setup needs it; the guided setup insists.
    Essential,
    /// Standard kit; the guided setup offers it.
    Recommended,
    /// Useful for specific goals; shown but never pushed.
    Optional,
}

impl Tier {
    pub fn describe(self) -> &'static str {
        match self {
            Tier::Essential => "essential",
            Tier::Recommended => "recommended",
            Tier::Optional => "optional",
        }
    }
}

/// Where a tool's files live.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Target {
    /// Into the game root (script extenders, RED4ext, SMAPI).
    GameRoot,
    /// Into the game's mod root (`Data/` for Bethesda games).
    ModRoot,
    /// Self-contained program in lmm's own tools directory (LOOT, xEdit).
    Standalone,
}

/// One tool in a game's catalog.
#[derive(Debug, Clone, Copy)]
pub struct ToolDef {
    /// Stable identifier used on the command line (`tools install skse`).
    pub id: &'static str,
    pub name: &'static str,
    /// One-line description shown in listings.
    pub summary: &'static str,
    pub kind: ToolKind,
    pub tier: Tier,
    pub target: Target,
    /// Paths relative to the target root whose presence marks the tool
    /// installed even when lmm did not put it there. The final component
    /// may contain a single `*` wildcard. Empty = only installs managed
    /// by lmm are detectable (typical for standalone tools).
    pub detect: &'static [&'static str],
    /// Executable to start for `tools launch`, relative to the target root.
    pub exe: Option<&'static str>,
    /// Newest version known to this build of lmm; used for the offline
    /// "outdated" check against the recorded installed version.
    pub latest_known: Option<&'static str>,
    /// Where to download the tool.
    pub url: &'static str,
    /// Nexus (domain, mod id) when the tool is distributed on Nexus Mods —
    /// lets frontends point at the nxm:// download flow.
    pub nexus: Option<(&'static str, u32)>,
}

/// One INI setting a game needs for modding to work.
#[derive(Debug, Clone, Copy)]
pub struct TweakDef {
    pub id: &'static str,
    /// File name inside the game's INI directory (created if missing;
    /// matched case-insensitively because Proton prefixes live on
    /// case-sensitive filesystems).
    pub file: &'static str,
    pub section: &'static str,
    pub key: &'static str,
    /// Desired value; empty string means "key present and empty".
    pub value: &'static str,
    /// Why the change is needed, in user-facing language.
    pub why: &'static str,
}

/// Where a game keeps its INI files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IniDir {
    /// `Documents/My Games/<name>` inside the Proton prefix's user profile.
    MyGames(&'static str),
    /// In the game root itself (Morrowind).
    GameRoot,
}

/// Where and how a game records its plugin load order.
#[derive(Debug, Clone, Copy)]
pub struct PluginsSpec {
    /// Directory under `AppData/Local/` (in the Proton prefix) holding
    /// `plugins.txt`.
    pub local_dir: &'static str,
    /// Modern format: `*Plugin.esp` = enabled, bare name = installed but
    /// disabled. Older games list only enabled plugins, unprefixed.
    pub asterisk: bool,
    /// Official plugins in their fixed load positions; always sorted first,
    /// in this order.
    pub official: &'static [&'static str],
}

/// Everything the Game Tools section knows about one game.
pub struct GameTools {
    pub slug: &'static str,
    pub tools: &'static [ToolDef],
    pub tweaks: &'static [TweakDef],
    pub ini_dir: Option<IniDir>,
    pub plugins: Option<PluginsSpec>,
}

/// Catalog lookup; `None` means lmm has no tool knowledge for the game
/// (e.g. `generic`), not that the game is unsupported.
pub fn for_game(slug: &str) -> Option<&'static GameTools> {
    CATALOG.iter().find(|g| g.slug == slug)
}

/// A tool from a game's catalog by id, or unambiguous id/name prefix.
pub fn find_tool(game: &GameTools, selector: &str) -> Option<&'static ToolDef> {
    let sel = selector.to_lowercase();
    if let Some(t) = game.tools.iter().find(|t| t.id == sel) {
        return Some(t);
    }
    let matches: Vec<&ToolDef> = game
        .tools
        .iter()
        .filter(|t| t.id.starts_with(&sel) || t.name.to_lowercase().starts_with(&sel))
        .collect();
    match matches[..] {
        [one] => Some(one),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Catalog data. Sources: the standard community setup for each game as of
// this build. `latest_known` values are best-effort snapshots — a newer
// release only makes lmm's "outdated" check conservative, never wrong about
// what is installed.

const SKYRIMSE_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "skse",
        name: "SKSE64",
        summary: "Skyrim Script Extender — required by most script-heavy mods",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["skse64_loader.exe"],
        exe: None,
        latest_known: Some("2.2.6"),
        url: "https://skse.silverlock.org/",
        nexus: Some(("skyrimspecialedition", 30379)),
    },
    ToolDef {
        id: "address-library",
        name: "Address Library for SKSE Plugins",
        summary: "Version database nearly every SKSE plugin depends on",
        kind: ToolKind::ExtenderPlugin,
        tier: Tier::Essential,
        target: Target::ModRoot,
        detect: &["SKSE/Plugins/version-*.bin"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/skyrimspecialedition/mods/32444",
        nexus: Some(("skyrimspecialedition", 32444)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
    ToolDef {
        id: "sseedit",
        name: "SSEEdit (xEdit)",
        summary: "Plugin editor for conflict inspection and patch making",
        kind: ToolKind::Editor,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("SSEEdit.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/skyrimspecialedition/mods/164",
        nexus: Some(("skyrimspecialedition", 164)),
    },
    ToolDef {
        id: "bodyslide",
        name: "BodySlide and Outfit Studio",
        summary: "Builds body and outfit meshes for body-replacer setups",
        kind: ToolKind::Builder,
        tier: Tier::Recommended,
        target: Target::ModRoot,
        detect: &["CalienteTools/BodySlide/BodySlide x64.exe"],
        exe: Some("CalienteTools/BodySlide/BodySlide x64.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/skyrimspecialedition/mods/201",
        nexus: Some(("skyrimspecialedition", 201)),
    },
    ToolDef {
        id: "pandora",
        name: "Pandora Behaviour Engine+",
        summary: "Behavior patcher for animation mods (Nemesis successor)",
        kind: ToolKind::Builder,
        tier: Tier::Optional,
        target: Target::Standalone,
        detect: &[],
        exe: Some("Pandora Behaviour Engine+.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/skyrimspecialedition/mods/133232",
        nexus: Some(("skyrimspecialedition", 133232)),
    },
    ToolDef {
        id: "dyndolod",
        name: "DynDOLOD",
        summary: "Generates distant-object LOD for a modded load order",
        kind: ToolKind::Builder,
        tier: Tier::Optional,
        target: Target::Standalone,
        detect: &[],
        exe: Some("DynDOLOD64.exe"),
        latest_known: None,
        url: "https://dyndolod.info/",
        nexus: Some(("skyrimspecialedition", 32382)),
    },
];

const SKYRIMSE_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "Skyrim Special Edition",
    asterisk: true,
    official: &[
        "Skyrim.esm",
        "Update.esm",
        "Dawnguard.esm",
        "HearthFires.esm",
        "Dragonborn.esm",
    ],
};

const FALLOUT4_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "f4se",
        name: "F4SE",
        summary: "Fallout 4 Script Extender — required by most script-heavy mods",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["f4se_loader.exe"],
        exe: None,
        latest_known: Some("0.7.2"),
        url: "https://f4se.silverlock.org/",
        nexus: Some(("fallout4", 42147)),
    },
    ToolDef {
        id: "address-library",
        name: "Address Library for F4SE Plugins",
        summary: "Version database most F4SE plugins depend on",
        kind: ToolKind::ExtenderPlugin,
        tier: Tier::Essential,
        target: Target::ModRoot,
        detect: &["F4SE/Plugins/version-*.bin"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/fallout4/mods/47327",
        nexus: Some(("fallout4", 47327)),
    },
    ToolDef {
        id: "fo4edit",
        name: "FO4Edit (xEdit)",
        summary: "Plugin editor for conflict inspection and patch making",
        kind: ToolKind::Editor,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("FO4Edit.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/fallout4/mods/2737",
        nexus: Some(("fallout4", 2737)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
    ToolDef {
        id: "bodyslide",
        name: "BodySlide and Outfit Studio",
        summary: "Builds body and outfit meshes for body-replacer setups",
        kind: ToolKind::Builder,
        tier: Tier::Recommended,
        target: Target::ModRoot,
        detect: &["Tools/BodySlide/BodySlide x64.exe"],
        exe: Some("Tools/BodySlide/BodySlide x64.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/fallout4/mods/25",
        nexus: Some(("fallout4", 25)),
    },
];

const FALLOUT4_TWEAKS: &[TweakDef] = &[
    TweakDef {
        id: "invalidation",
        file: "Fallout4Custom.ini",
        section: "Archive",
        key: "bInvalidateOlderFiles",
        value: "1",
        why: "lets the engine load loose mod files that are older than the game's archives",
    },
    TweakDef {
        id: "loose-files",
        file: "Fallout4Custom.ini",
        section: "Archive",
        key: "sResourceDataDirsFinal",
        value: "",
        why: "clears the resource whitelist so loose files load from every Data subdirectory",
    },
    TweakDef {
        id: "mod-selection",
        file: "Fallout4Prefs.ini",
        section: "Launcher",
        key: "bEnableFileSelection",
        value: "1",
        why: "allows plugins other than the official ones to be enabled",
    },
];

const FALLOUT4_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "Fallout4",
    asterisk: true,
    official: &[
        "Fallout4.esm",
        "DLCRobot.esm",
        "DLCworkshop01.esm",
        "DLCCoast.esm",
        "DLCworkshop02.esm",
        "DLCworkshop03.esm",
        "DLCNukaWorld.esm",
        "DLCUltraHighResolution.esm",
    ],
};

const FALLOUTNV_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "xnvse",
        name: "xNVSE",
        summary: "New Vegas Script Extender (community-maintained NVSE)",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["nvse_loader.exe"],
        exe: None,
        latest_known: Some("6.3.5"),
        url: "https://github.com/xNVSE/NVSE/releases",
        nexus: Some(("newvegas", 67883)),
    },
    ToolDef {
        id: "jip-ln-nvse",
        name: "JIP LN NVSE Plugin",
        summary: "Engine-extension plugin nearly every modern NV mod needs",
        kind: ToolKind::ExtenderPlugin,
        tier: Tier::Essential,
        target: Target::ModRoot,
        detect: &["NVSE/Plugins/jip_nvse.dll"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/newvegas/mods/58277",
        nexus: Some(("newvegas", 58277)),
    },
    ToolDef {
        id: "johnnyguitar",
        name: "JohnnyGuitar NVSE",
        summary: "Companion engine-extension plugin to JIP LN",
        kind: ToolKind::ExtenderPlugin,
        tier: Tier::Recommended,
        target: Target::ModRoot,
        detect: &["NVSE/Plugins/johnnyguitar.dll"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/newvegas/mods/66927",
        nexus: Some(("newvegas", 66927)),
    },
    ToolDef {
        id: "fnvedit",
        name: "FNVEdit (xEdit)",
        summary: "Plugin editor for conflict inspection and patch making",
        kind: ToolKind::Editor,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("FNVEdit.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/newvegas/mods/34703",
        nexus: Some(("newvegas", 34703)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
];

/// Archive invalidation for the Gamebryo generation (FNV/FO3/Oblivion).
const GAMEBRYO_TWEAKS_FNV: &[TweakDef] = &[
    TweakDef {
        id: "invalidation",
        file: "Fallout.ini",
        section: "Archive",
        key: "bInvalidateOlderFiles",
        value: "1",
        why: "archive invalidation: lets loose mod files override the game's packed assets",
    },
    TweakDef {
        id: "invalidation-file",
        file: "Fallout.ini",
        section: "Archive",
        key: "SInvalidationFile",
        value: "",
        why: "disables the obsolete invalidation-file mechanism that fights the setting above",
    },
];

const FALLOUTNV_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "FalloutNV",
    asterisk: false,
    official: &[
        "FalloutNV.esm",
        "DeadMoney.esm",
        "HonestHearts.esm",
        "OldWorldBlues.esm",
        "LonesomeRoad.esm",
        "GunRunnersArsenal.esm",
        "ClassicPack.esm",
        "MercenaryPack.esm",
        "TribalPack.esm",
        "CaravanPack.esm",
    ],
};

const FALLOUT3_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "fose",
        name: "FOSE",
        summary: "Fallout Script Extender for Fallout 3",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["fose_loader.exe"],
        exe: None,
        latest_known: None,
        url: "https://fose.silverlock.org/",
        nexus: Some(("fallout3", 8606)),
    },
    ToolDef {
        id: "fo3edit",
        name: "FO3Edit (xEdit)",
        summary: "Plugin editor for conflict inspection and patch making",
        kind: ToolKind::Editor,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("FO3Edit.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/fallout3/mods/637",
        nexus: Some(("fallout3", 637)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
];

const FALLOUT3_TWEAKS: &[TweakDef] = &[
    TweakDef {
        id: "invalidation",
        file: "FALLOUT.INI",
        section: "Archive",
        key: "bInvalidateOlderFiles",
        value: "1",
        why: "archive invalidation: lets loose mod files override the game's packed assets",
    },
    TweakDef {
        id: "invalidation-file",
        file: "FALLOUT.INI",
        section: "Archive",
        key: "SInvalidationFile",
        value: "",
        why: "disables the obsolete invalidation-file mechanism that fights the setting above",
    },
];

const FALLOUT3_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "Fallout3",
    asterisk: false,
    official: &[
        "Fallout3.esm",
        "Anchorage.esm",
        "ThePitt.esm",
        "BrokenSteel.esm",
        "PointLookout.esm",
        "Zeta.esm",
    ],
};

const SKYRIM_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "skse",
        name: "SKSE",
        summary: "Skyrim Script Extender for the original (Legendary) edition",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["skse_loader.exe"],
        exe: None,
        latest_known: Some("1.7.3"),
        url: "https://skse.silverlock.org/",
        nexus: None,
    },
    ToolDef {
        id: "tes5edit",
        name: "TES5Edit (xEdit)",
        summary: "Plugin editor for conflict inspection and patch making",
        kind: ToolKind::Editor,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("TES5Edit.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/skyrim/mods/25859",
        nexus: Some(("skyrim", 25859)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
];

const SKYRIM_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "Skyrim",
    asterisk: false,
    official: &[
        "Skyrim.esm",
        "Update.esm",
        "Dawnguard.esm",
        "HearthFires.esm",
        "Dragonborn.esm",
    ],
};

const SKYRIMVR_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "sksevr",
        name: "SKSEVR",
        summary: "Skyrim Script Extender for Skyrim VR",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["sksevr_loader.exe"],
        exe: None,
        latest_known: Some("2.0.12"),
        url: "https://skse.silverlock.org/",
        nexus: None,
    },
    ToolDef {
        id: "vr-address-library",
        name: "VR Address Library for SKSEVR",
        summary: "Version database SKSEVR plugins depend on",
        kind: ToolKind::ExtenderPlugin,
        tier: Tier::Essential,
        target: Target::ModRoot,
        detect: &["SKSE/Plugins/version-*.csv"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/skyrimspecialedition/mods/58101",
        nexus: Some(("skyrimspecialedition", 58101)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
    ToolDef {
        id: "sseedit",
        name: "SSEEdit (xEdit)",
        summary: "Plugin editor for conflict inspection and patch making",
        kind: ToolKind::Editor,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("SSEEdit.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/skyrimspecialedition/mods/164",
        nexus: Some(("skyrimspecialedition", 164)),
    },
];

const SKYRIMVR_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "Skyrim VR",
    asterisk: true,
    official: &[
        "Skyrim.esm",
        "Update.esm",
        "Dawnguard.esm",
        "HearthFires.esm",
        "Dragonborn.esm",
        "SkyrimVR.esm",
    ],
};

const OBLIVION_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "obse",
        name: "OBSE",
        summary: "Oblivion Script Extender",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["obse_loader.exe"],
        exe: None,
        latest_known: None,
        url: "https://obse.silverlock.org/",
        nexus: None,
    },
    ToolDef {
        id: "tes4edit",
        name: "TES4Edit (xEdit)",
        summary: "Plugin editor for conflict inspection and patch making",
        kind: ToolKind::Editor,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("TES4Edit.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/oblivion/mods/11536",
        nexus: Some(("oblivion", 11536)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
];

const OBLIVION_TWEAKS: &[TweakDef] = &[
    TweakDef {
        id: "invalidation",
        file: "Oblivion.ini",
        section: "Archive",
        key: "bInvalidateOlderFiles",
        value: "1",
        why: "archive invalidation: lets loose mod files override the game's packed assets",
    },
    TweakDef {
        id: "invalidation-file",
        file: "Oblivion.ini",
        section: "Archive",
        key: "SInvalidationFile",
        value: "",
        why: "disables the obsolete invalidation-file mechanism that fights the setting above",
    },
];

const OBLIVION_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "Oblivion",
    asterisk: false,
    official: &["Oblivion.esm"],
};

const MORROWIND_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "mge-xe",
        name: "MGE XE",
        summary: "Graphics extender; also bundles the MWSE script extender",
        kind: ToolKind::Framework,
        tier: Tier::Recommended,
        target: Target::GameRoot,
        detect: &["MGEXEgui.exe"],
        exe: Some("MGEXEgui.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/morrowind/mods/41102",
        nexus: Some(("morrowind", 41102)),
    },
    ToolDef {
        id: "mwse",
        name: "MWSE",
        summary: "Morrowind Script Extender (installed/updated by MGE XE)",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Optional,
        target: Target::GameRoot,
        detect: &["MWSE.dll"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/morrowind/mods/45468",
        nexus: Some(("morrowind", 45468)),
    },
];

const STARFIELD_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "sfse",
        name: "SFSE",
        summary: "Starfield Script Extender",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["sfse_loader.exe"],
        exe: None,
        latest_known: None,
        url: "https://github.com/ianpatt/sfse/releases",
        nexus: Some(("starfield", 106)),
    },
    ToolDef {
        id: "address-library",
        name: "Address Library for SFSE Plugins",
        summary: "Version database SFSE plugins depend on",
        kind: ToolKind::ExtenderPlugin,
        tier: Tier::Essential,
        target: Target::ModRoot,
        detect: &["SFSE/Plugins/version-*.bin"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/starfield/mods/3256",
        nexus: Some(("starfield", 3256)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
];

const STARFIELD_TWEAKS: &[TweakDef] = &[
    TweakDef {
        id: "invalidation",
        file: "StarfieldCustom.ini",
        section: "Archive",
        key: "bInvalidateOlderFiles",
        value: "1",
        why: "lets the engine load loose mod files that are older than the game's archives",
    },
    TweakDef {
        id: "loose-files",
        file: "StarfieldCustom.ini",
        section: "Archive",
        key: "sResourceDataDirsFinal",
        value: "",
        why: "clears the resource whitelist so loose files load from every Data subdirectory",
    },
];

const STARFIELD_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "Starfield",
    asterisk: true,
    official: &[
        "Starfield.esm",
        "Constellation.esm",
        "OldMars.esm",
        "BlueprintShips-Starfield.esm",
    ],
};

const ENDERALSE_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "skse",
        name: "SKSE64",
        summary: "Skyrim Script Extender — Enderal SE runs on the Skyrim SE engine",
        kind: ToolKind::ScriptExtender,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["skse64_loader.exe"],
        exe: None,
        latest_known: Some("2.0.20"),
        url: "https://skse.silverlock.org/",
        nexus: None,
    },
    ToolDef {
        id: "address-library",
        name: "Address Library for SKSE Plugins",
        summary: "Version database nearly every SKSE plugin depends on",
        kind: ToolKind::ExtenderPlugin,
        tier: Tier::Essential,
        target: Target::ModRoot,
        detect: &["SKSE/Plugins/version-*.bin"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/skyrimspecialedition/mods/32444",
        nexus: Some(("skyrimspecialedition", 32444)),
    },
    ToolDef {
        id: "loot",
        name: "LOOT",
        summary: "Load-order optimizer with community masterlist rules",
        kind: ToolKind::LoadOrderTool,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("LOOT.exe"),
        latest_known: None,
        url: "https://loot.github.io/",
        nexus: None,
    },
    ToolDef {
        id: "sseedit",
        name: "SSEEdit (xEdit)",
        summary: "Plugin editor for conflict inspection and patch making",
        kind: ToolKind::Editor,
        tier: Tier::Recommended,
        target: Target::Standalone,
        detect: &[],
        exe: Some("SSEEdit.exe"),
        latest_known: None,
        url: "https://www.nexusmods.com/skyrimspecialedition/mods/164",
        nexus: Some(("skyrimspecialedition", 164)),
    },
];

const ENDERALSE_PLUGINS: PluginsSpec = PluginsSpec {
    local_dir: "Enderal Special Edition",
    asterisk: true,
    official: &[
        "Skyrim.esm",
        "Update.esm",
        "Dawnguard.esm",
        "HearthFires.esm",
        "Dragonborn.esm",
        "Enderal - Forgotten Stories.esm",
    ],
};

const STARDEW_TOOLS: &[ToolDef] = &[ToolDef {
    id: "smapi",
    name: "SMAPI",
    summary: "Stardew Modding API — the mod loader every SDV mod requires",
    kind: ToolKind::Framework,
    tier: Tier::Essential,
    target: Target::GameRoot,
    detect: &["StardewModdingAPI"],
    exe: None,
    latest_known: None,
    url: "https://smapi.io/",
    nexus: Some(("stardewvalley", 2400)),
}];

const CYBERPUNK_TOOLS: &[ToolDef] = &[
    ToolDef {
        id: "red4ext",
        name: "RED4ext",
        summary: "Script-extender/plugin loader most framework mods require",
        kind: ToolKind::Framework,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["red4ext/RED4ext.dll"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/cyberpunk2077/mods/2380",
        nexus: Some(("cyberpunk2077", 2380)),
    },
    ToolDef {
        id: "cet",
        name: "Cyber Engine Tweaks",
        summary: "Lua scripting framework and in-game console",
        kind: ToolKind::Framework,
        tier: Tier::Essential,
        target: Target::GameRoot,
        detect: &["bin/x64/plugins/cyber_engine_tweaks.asi"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/cyberpunk2077/mods/107",
        nexus: Some(("cyberpunk2077", 107)),
    },
    ToolDef {
        id: "redscript",
        name: "redscript",
        summary: "Script compiler for mods that patch game scripts",
        kind: ToolKind::Framework,
        tier: Tier::Recommended,
        target: Target::GameRoot,
        detect: &["engine/tools/scc.exe"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/cyberpunk2077/mods/1511",
        nexus: Some(("cyberpunk2077", 1511)),
    },
    ToolDef {
        id: "archivexl",
        name: "ArchiveXL",
        summary: "Resource loader for mods that add rather than replace assets",
        kind: ToolKind::Framework,
        tier: Tier::Recommended,
        target: Target::GameRoot,
        detect: &["red4ext/plugins/ArchiveXL/ArchiveXL.dll"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/cyberpunk2077/mods/4198",
        nexus: Some(("cyberpunk2077", 4198)),
    },
    ToolDef {
        id: "tweakxl",
        name: "TweakXL",
        summary: "TweakDB modification framework",
        kind: ToolKind::Framework,
        tier: Tier::Recommended,
        target: Target::GameRoot,
        detect: &["red4ext/plugins/TweakXL/TweakXL.dll"],
        exe: None,
        latest_known: None,
        url: "https://www.nexusmods.com/cyberpunk2077/mods/4197",
        nexus: Some(("cyberpunk2077", 4197)),
    },
];

const CATALOG: &[GameTools] = &[
    GameTools {
        slug: "skyrimse",
        tools: SKYRIMSE_TOOLS,
        tweaks: &[],
        ini_dir: Some(IniDir::MyGames("Skyrim Special Edition")),
        plugins: Some(SKYRIMSE_PLUGINS),
    },
    GameTools {
        slug: "skyrim",
        tools: SKYRIM_TOOLS,
        tweaks: &[],
        ini_dir: Some(IniDir::MyGames("Skyrim")),
        plugins: Some(SKYRIM_PLUGINS),
    },
    GameTools {
        slug: "skyrimvr",
        tools: SKYRIMVR_TOOLS,
        tweaks: &[],
        ini_dir: Some(IniDir::MyGames("Skyrim VR")),
        plugins: Some(SKYRIMVR_PLUGINS),
    },
    GameTools {
        slug: "fallout4",
        tools: FALLOUT4_TOOLS,
        tweaks: FALLOUT4_TWEAKS,
        ini_dir: Some(IniDir::MyGames("Fallout4")),
        plugins: Some(FALLOUT4_PLUGINS),
    },
    GameTools {
        slug: "falloutnv",
        tools: FALLOUTNV_TOOLS,
        tweaks: GAMEBRYO_TWEAKS_FNV,
        ini_dir: Some(IniDir::MyGames("FalloutNV")),
        plugins: Some(FALLOUTNV_PLUGINS),
    },
    GameTools {
        slug: "fallout3",
        tools: FALLOUT3_TOOLS,
        tweaks: FALLOUT3_TWEAKS,
        ini_dir: Some(IniDir::MyGames("Fallout3")),
        plugins: Some(FALLOUT3_PLUGINS),
    },
    GameTools {
        slug: "oblivion",
        tools: OBLIVION_TOOLS,
        tweaks: OBLIVION_TWEAKS,
        ini_dir: Some(IniDir::MyGames("Oblivion")),
        plugins: Some(OBLIVION_PLUGINS),
    },
    GameTools {
        slug: "morrowind",
        tools: MORROWIND_TOOLS,
        tweaks: &[],
        ini_dir: Some(IniDir::GameRoot),
        plugins: None,
    },
    GameTools {
        slug: "starfield",
        tools: STARFIELD_TOOLS,
        tweaks: STARFIELD_TWEAKS,
        ini_dir: Some(IniDir::MyGames("Starfield")),
        plugins: Some(STARFIELD_PLUGINS),
    },
    GameTools {
        slug: "enderalse",
        tools: ENDERALSE_TOOLS,
        tweaks: &[],
        ini_dir: Some(IniDir::MyGames("Enderal Special Edition")),
        plugins: Some(ENDERALSE_PLUGINS),
    },
    GameTools {
        slug: "stardewvalley",
        tools: STARDEW_TOOLS,
        tweaks: &[],
        ini_dir: None,
        plugins: None,
    },
    GameTools {
        slug: "cyberpunk2077",
        tools: CYBERPUNK_TOOLS,
        tweaks: &[],
        ini_dir: None,
        plugins: None,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_slugs_exist_in_game_registry_and_are_unique() {
        let mut slugs: Vec<_> = CATALOG.iter().map(|g| g.slug).collect();
        slugs.sort_unstable();
        slugs.dedup();
        assert_eq!(slugs.len(), CATALOG.len(), "duplicate catalog slug");
        for g in CATALOG {
            assert!(
                crate::games::by_slug(g.slug).is_some(),
                "catalog slug '{}' missing from game registry",
                g.slug
            );
        }
    }

    #[test]
    fn tool_ids_unique_per_game_and_wildcards_valid() {
        for g in CATALOG {
            let mut ids: Vec<_> = g.tools.iter().map(|t| t.id).collect();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), g.tools.len(), "duplicate tool id in {}", g.slug);
            for t in g.tools {
                for d in t.detect {
                    // Wildcards are only supported in the final component.
                    let (dir, _last) = d.rsplit_once('/').unwrap_or(("", d));
                    assert!(!dir.contains('*'), "{}: wildcard in non-final component", d);
                    assert!(
                        d.matches('*').count() <= 1,
                        "{}: at most one wildcard supported",
                        d
                    );
                }
            }
        }
    }

    #[test]
    fn find_tool_resolves_ids_and_prefixes() {
        let g = for_game("skyrimse").unwrap();
        assert_eq!(find_tool(g, "skse").unwrap().id, "skse");
        assert_eq!(find_tool(g, "addr").unwrap().id, "address-library");
        assert_eq!(find_tool(g, "LOOT").unwrap().id, "loot");
        assert!(find_tool(g, "nope").is_none());
        // "s" prefixes skse and sseedit both -> ambiguous.
        assert!(find_tool(g, "s").is_none());
    }

    #[test]
    fn unknown_games_have_no_catalog() {
        assert!(for_game("generic").is_none());
        assert!(for_game("bannerlord").is_none());
    }
}
