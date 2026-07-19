# Game Tools

The Game Tools section (`lmm tools`) is the central place for preparing,
maintaining and troubleshooting a game's modding environment: the utilities
and one-time setup tasks that are essential for modding but are not ordinary
mods. The goal is that a new user can install a game, register it with lmm,
run `tools setup`, and start installing mods — without hunting through
external guides for "what do I need for Skyrim modding".

Everything lives under one command family:

```
lmm tools                          tool catalog for the current game, with status
lmm tools install <tool> [archive] install/update a tool ('update' is an alias)
lmm tools verify [tool]            re-hash managed tool files against the manifest
lmm tools launch <tool>            start a tool (Windows tools run through Proton)
lmm tools remove <tool>            remove a tool, restoring displaced originals
lmm tools setup                    guided first-time setup
lmm tools check                    "ready for modding?" checklist
lmm tools config show|apply|restore   modding-required INI settings
lmm tools loadorder [sort|backups|restore]   plugin load-order maintenance
```

All commands honor `--json`, `--yes`, and `--game <install>` like the rest of
lmm, and work identically in the interactive shell and as one-shot calls.

## Per-game catalogs

lmm ships a built-in catalog (`lmm-core/src/tools/registry.rs`) of the
community-standard toolkit per game, so the section only ever shows what is
relevant to the *current* game. Tools are tiered:

- **essential** — practically every modded setup needs it (SKSE, Address
  Library, xNVSE + JIP LN, SMAPI, RED4ext/CET…). `tools setup` insists,
  `tools check` fails without them.
- **recommended** — standard kit (LOOT, the xEdit family, BodySlide).
- **optional** — goal-specific (Pandora, DynDOLOD). Shown, never pushed.

Catalogs exist for the Bethesda family (Skyrim SE/LE/VR, Oblivion, Morrowind,
Fallout 3/NV/4, Starfield, Enderal SE), Stardew Valley and Cyberpunk 2077.
Games without a catalog (e.g. `generic`) get a clear message instead of an
empty screen. Adding a game or tool is adding a data row to the registry.

## Tool status and management

Every tool shows one of four states:

- **installed** — managed by lmm (full manifest recorded), or detected on
  disk from a pre-lmm/manual installation ("found on disk").
- **missing** — not present; the listing shows where to download it.
- **outdated** — the recorded version is older than the newest version this
  lmm build knows about (an offline check; a newer release than lmm knows
  simply isn't flagged).
- **attention** — something is off: manifest files missing on disk, or only
  part of the expected files present.

`tools install <tool> <archive>` installs from a downloaded `.zip`/`.7z`
(the same validated, bounded extraction pipeline as mods; solitary wrapper
directories like `skse64_2_02_06/` are unwrapped using the tool's known file
layout as the anchor). Depending on the tool it targets:

- the **game root** (script extenders: `skse64_loader.exe`, …),
- the **mod root** (`Data/` payloads: Address Library, BodySlide), or
- a **standalone directory** under `~/.local/share/lmm/tools/` (LOOT, xEdit)
  that never touches the game at all.

The usual lmm safety rules apply: a full per-file manifest is recorded, any
unmanaged file a tool displaces is backed up first and restored on `tools
remove`, files modified outside lmm are never overwritten or deleted without
`--force`, a failed install undoes its completed steps, and a path currently
deployed by a *mod* is refused (install that as a mod instead). Without an
archive argument, `tools install <tool>` prints the download page (and the
Nexus mod when there is one) instead.

`tools launch` runs native tools directly; Windows tools run through the
game's own Proton prefix (newest installed Proton, `STEAM_COMPAT_*` set
appropriately). Script extenders are the exception — they start *instead of*
the game, so lmm explains the Steam launch-options approach rather than
pretending to launch them.

## Guided setup

`tools setup` walks the essential and recommended tools one by one: already
installed ones are ticked off, missing ones show their download page and
prompt for a downloaded archive path (Enter skips — everything is skippable),
then it offers to apply the game's configuration tweaks and sort the load
order. Non-interactive runs (`--yes`, piped stdin) apply configuration and
sorting and just report which tools still need archives.

## Game configuration

`tools config` covers the INI changes a game needs before mods work — the
things users otherwise edit by hand from decade-old forum posts:

- Fallout NV / Fallout 3 / Oblivion: archive invalidation
  (`bInvalidateOlderFiles=1`, clear `SInvalidationFile`).
- Fallout 4 / Starfield: `bInvalidateOlderFiles`, clear
  `sResourceDataDirsFinal` (loose files), `bEnableFileSelection=1` (FO4).
- Skyrim SE/LE/VR, Morrowind, native games: nothing needed — the section
  says so instead of inventing work.

`show` reports each setting as applied / not applied / file missing; `apply`
performs minimal, line-preserving edits (only the targeted key changes) and
backs up each file the first time lmm ever touches it — creating missing
files like `Fallout4Custom.ini` when needed; `restore` returns every touched
file to its byte-exact pre-lmm state (deleting files lmm created). INI files
are located inside the installation's Proton prefix
(`Documents/My Games/<game>`), with case-insensitive filename matching since
prefixes live on case-sensitive filesystems.

## Load order

`tools loadorder` manages `plugins.txt` (in the prefix under
`AppData/Local/<game>`), understanding both the modern `*Plugin.esp` format
(Skyrim SE+, Fallout 4, Starfield) and the older enabled-only list. lmm
parses each plugin's TES4 header (both the Oblivion-era and Skyrim-era
layouts) for its master list and flags, then:

- **analyze** (the default) lists the order and reports issues: files listed
  but missing from `Data/`, plugins loading before their masters, masters
  missing or disabled, duplicates, and plugin files present but never listed.
- **sort** applies a conservative best-practice order: official plugins
  first in their fixed positions, then masters, then regular plugins, every
  plugin after its masters, ties keeping their current order (a stable
  topological sort). The current order is backed up first, and enabled/
  disabled state survives the rewrite.
- **backups** / **restore** list and roll back to any earlier order (a
  restore also backs up the current order, so it is itself undoable).

This is deliberately maintenance-grade, not a LOOT replacement: it fixes the
orderings that break games, and the catalog offers LOOT right next to it for
masterlist-driven nuance. For pre-Skyrim-SE games the output notes that file
timestamps also affect load order.

## Health check

`tools check` runs everything above as one checklist: game directory intact,
no interrupted deployment pending, each catalog tool's status (essential
missing ⇒ FAIL, recommended missing ⇒ WARN), script extender detected,
configuration applied, plugin list parseable and issue-free. Every non-OK
line comes with a concrete recommendation ("run 'tools config apply'",
download URL, …), and checks that need a Proton prefix that doesn't exist
yet are reported as *skipped* with "run the game once through Steam" rather
than failing.

## Storage

| What | Where |
|---|---|
| Managed tool manifests | `tools` / `tool_files` tables (schema v4) |
| Standalone tools | `~/.local/share/lmm/tools/<install>/<tool>/` |
| Displaced-file backups | `~/.local/share/lmm/backups/tools/<install>/` |
| INI originals | `~/.local/share/lmm/backups/config/<install>/` + `config_backups` table |
| Load-order backups | `~/.local/share/lmm/loadorder/<install>/plugins-<ms>.txt` |

An installation cannot be unregistered while managed tools remain (mirroring
the deployed-files rule), so tool files in the game tree are never orphaned.
