# lmm — Linux Mod Manager

A Linux-native CLI mod manager for games, with initial focus on Nexus Mods-style
local archives and Steam/Proton game installations.

This document is the design reference for the project. It is written before the
implementation and updated as decisions change.

---

## 1. Architecture overview

Three crates in a Cargo workspace:

```
crates/
  lmm-core/   Library: all mod-management logic. No CLI concerns, no stdout.
  lmm-nexus/  Library: Nexus Mods integration — nxm:// link validation, API
              client, persistent download queue, browser-handler IPC, XDG
              registration. Interface-agnostic like lmm-core.
              (see docs/NEXUS.md)
  lmm-cli/    Binary `lmm`: argument parsing, output formatting (human + JSON),
              confirmation prompts, and the interactive shell — a readline
              loop over the same parser and dispatch layer as one-shot
              invocations (see docs/SHELL.md). Thin layer over the libraries.
```

`lmm-core` exposes a `Context` (config + database + data directories) and
domain modules. Every operation that a CLI command performs is a library
function that returns structured data. A future TUI/GUI links `lmm-core`
directly and never shells out to the CLI.

Design rules:

* Core functions never print, never prompt, never read env vars for behaviour.
  They take parameters and return results/errors.
* Anything destructive is split into **plan** (pure, returns what would happen)
  and **execute** (takes the plan). Dry-run = print the plan and stop.
* The database is the source of truth for *managed* state; the filesystem is
  the source of truth for *actual* state. `verify` diffs the two, `repair`
  reconciles them.

### Major components

| Component | Module(s) | Responsibility |
|---|---|---|
| Config | `config` | TOML config, XDG paths, limits, extra Steam roots |
| Database | `db` | rusqlite connection, migrations, typed queries |
| Game registry | `games` | Built-in definitions of supported games (layout rules, Steam app ids) |
| Discovery | `discovery` | Steam/Proton/Flatpak scanning, VDF parsing |
| Safe paths | `paths` | `RelPath` — validated, normalized relative paths |
| Archives | `archive` | ZIP/7z enumeration, validation, bounded extraction |
| Staging | `staging` | Per-mod staging directories under the data dir |
| Mods | `mods` | Install from archive, inventory, remove |
| Profiles | `profile` | Profile CRUD, enabled state, priority (load order) |
| Resolution | `resolve` | Desired state: winning provider per file, conflicts |
| Deployment | `deploy` | Plan/execute/journal/backup/purge/verify/repair/rollback |
| Launch | `launch` | Start game via Steam URL or direct exec |
| NXM links | `lmm-nexus::nxm` | Strict parsing/validation of untrusted nxm:// URLs |
| Nexus API | `lmm-nexus::api` | Mod/file metadata, download-link generation |
| Download queue | `lmm-nexus::queue` | Persistent queue rows (`downloads` table), status transitions |
| Downloader | `lmm-nexus::download` | Size-capped, hashed, atomic archive transfer |
| Handler IPC | `lmm-nexus::ipc` | Unix socket to a running shell + on-disk spool fallback |
| XDG handler | `lmm-nexus::xdg` | Desktop-file registration for x-scheme-handler/nxm |
| Shell | `lmm-cli::shell` | Readline loop over the shared parser/dispatch (docs/SHELL.md) |
| Game Tools | `tools` | Per-game tool catalogs, managed tool installs, INI tweaks, plugin load order, health check (docs/TOOLS.md) |

---

## 2. Data directories

Default locations (overridable via config / CLI flags):

```
~/.config/lmm/config.toml       configuration
~/.local/share/lmm/
  lmm.db                        SQLite database
  staging/<mod_id>/             extracted, validated mod files (canonical copies)
  backups/<installation_id>/    original game files displaced by deployment
  tmp/                          extraction scratch (same fs as staging => cheap rename)
```

Staging is the canonical copy of every installed mod. Deployment copies from
staging into the game directory; the game directory is always reconstructible
from staging + backups.

---

## 3. Database model

SQLite via rusqlite, WAL mode, foreign keys ON, versioned migrations in a
`schema_version` pragma.

```sql
-- A game we know how to mod (mostly built-in registry data, persisted so that
-- installations can reference it stably).
games(
  id INTEGER PK,
  slug TEXT UNIQUE,          -- 'skyrimse', 'generic'
  name TEXT,
  steam_app_id INTEGER NULL
)

-- A concrete installed copy of a game on disk.
installations(
  id INTEGER PK,
  game_id -> games,
  path TEXT,                 -- absolute game root
  source TEXT,               -- 'steam' | 'manual'
  steam_library TEXT NULL,   -- library root it was found in
  proton_prefix TEXT NULL,   -- steamapps/compatdata/<appid>/pfx if present
  label TEXT NULL,
  created_at INTEGER,
  UNIQUE(path)
)

profiles(
  id INTEGER PK,
  installation_id -> installations,
  name TEXT,
  created_at INTEGER,
  UNIQUE(installation_id, name)
)
-- exactly one active profile per installation:
installations.active_profile_id -> profiles (nullable FK, set on first profile)

-- An installed mod (per installation, shared across that installation's profiles).
mods(
  id INTEGER PK,
  installation_id -> installations,
  name TEXT,
  version TEXT NULL,
  archive_name TEXT,         -- original archive filename
  archive_sha256 TEXT,       -- identity of the source archive
  staging_dir TEXT,          -- relative to data_dir/staging
  nexus_mod_id INTEGER NULL, -- future Nexus integration
  installed_at INTEGER,
  UNIQUE(installation_id, name)
)

-- Every file a mod supplies (relative to the game's mod target root).
mod_files(
  id INTEGER PK,
  mod_id -> mods,
  rel_path TEXT,             -- original casing, '/'-separated
  path_key TEXT,             -- lowercased rel_path: conflict/uniqueness key
  size INTEGER,
  sha256 TEXT,
  UNIQUE(mod_id, path_key)
)

-- Per-profile mod state. priority: higher wins (later in load order).
profile_mods(
  profile_id -> profiles,
  mod_id -> mods,
  enabled INTEGER,
  priority INTEGER,
  PRIMARY KEY(profile_id, mod_id)
)

-- Current live deployment state: what lmm believes is in the game directory.
-- Only mutated inside a committed deployment transaction.
deployed_files(
  id INTEGER PK,
  installation_id -> installations,
  path_key TEXT,
  rel_path TEXT,             -- casing actually used on disk
  provider_mod_id -> mods,
  sha256 TEXT,               -- hash of the file as deployed
  backup_id -> backups NULL, -- backup of the file we displaced, if any
  UNIQUE(installation_id, path_key)
)

-- Original (unmanaged) files moved aside by deployment.
backups(
  id INTEGER PK,
  installation_id -> installations,
  path_key TEXT,
  rel_path TEXT,
  backup_path TEXT,          -- relative to data_dir/backups
  sha256 TEXT,
  created_at INTEGER,
  UNIQUE(installation_id, path_key)
)

-- One row per deploy/purge run.
deployments(
  id INTEGER PK,
  installation_id -> installations,
  profile_id -> profiles NULL,
  kind TEXT,                 -- 'deploy' | 'purge'
  status TEXT,               -- 'running' | 'committed' | 'rolled_back'
  started_at INTEGER, finished_at INTEGER NULL
)

-- Write-ahead intent journal for filesystem operations (crash recovery).
journal(
  id INTEGER PK,
  deployment_id -> deployments,
  seq INTEGER,
  op TEXT,                   -- 'backup' | 'write' | 'remove' | 'restore' | 'mkdir'
  rel_path TEXT, path_key TEXT,
  mod_id INTEGER NULL,
  backup_id INTEGER NULL,
  pre_sha256 TEXT NULL,      -- expected hash of file being replaced/removed
  new_sha256 TEXT NULL,      -- hash of file being written
  state TEXT                 -- 'pending' | 'done' | 'undone'
)

settings(key TEXT PK, value TEXT)   -- e.g. default_installation
```

The schema answers the required questions:

* *Which mod owns this file?* — `mod_files` by `path_key`; live owner in `deployed_files`.
* *Which mods conflict?* — group `mod_files` of enabled mods by `path_key`, count > 1.
* *Which mod currently wins?* — max priority among enabled providers (resolution), or
  `deployed_files.provider_mod_id` for what is on disk right now.
* *What changes if a mod is disabled?* — recompute desired state without it, diff
  against `deployed_files` (this is exactly `deploy --dry-run` after the change).
* *What was modified during deployment?* — `journal` rows of that deployment.

---

## 4. Conflict-resolution model

* Conflict key: `path_key` = the relative path lowercased (Windows games under
  Proton resolve paths case-insensitively; two mods shipping `Textures/x.dds`
  and `textures/x.dds` are the same file to the game).
* Each enabled mod in the active profile has a unique integer `priority`.
  **Higher priority wins** — matches "later in the load order overrides".
* Desired state = for every `path_key` provided by any enabled mod, the file
  from the highest-priority provider.
* `conflicts` lists every path with >1 enabled provider, showing the winner and
  the losers. Disabling/reordering changes only the desired state; nothing
  touches disk until `deploy`.

---

## 5. Deployment flow

`deploy` reconciles the game directory with the desired state. Files are
placed by copy (the default) or, with `[deploy] method = "hardlink"` in
config.toml, by hard link: the staged file is verified against its recorded
hash, linked to `<target>.lmm-tmp`, and renamed over the target — the same
atomicity as the copy path, but instant and without duplicating disk space.
Hard links only exist within one filesystem, so any file that cannot be
linked (game and staging on different disks, filesystem without hardlink
support) silently falls back to a verified copy; the outcome reports how many
files were linked vs copied. Caveat, documented to the user: a game or tool
that rewrites a linked file *in place* also rewrites the staged copy.
`verify` reports that as drift on both sides; repair then requires
reinstalling the mod. Everything below is method-independent:

1. **Resolve** desired state from the active profile (pure, in-memory).
2. **Plan** — diff desired vs `deployed_files`:
   * `Install`  — path not currently deployed.
   * `Replace`  — deployed but provider or content hash differs.
   * `Remove`   — deployed but no enabled mod provides it anymore.
   Preflight for each op:
   * staged source exists and matches its `mod_files.sha256`;
   * if the target exists and `deployed_files` says we own it, its hash must
     match — otherwise it was modified externally: the plan flags it and
     execution refuses without `--force`;
   * if the target exists and we do *not* own it, it is an original game file:
     plan a `backup` before the write.
3. **Dry-run** stops here and prints the plan (also as JSON).
4. **Execute**:
   * insert `deployments` row (`running`) and all journal rows (`pending`)
     in one SQLite transaction — the intent log is durable before any
     filesystem change;
   * perform ops in order, marking journal rows `done` as they complete:
     - `backup`: **move** the original into `backups/` (move, not copy —
       guarantees the write below can't race a half-copied backup);
     - `write`: copy staged file to `<target>.lmm-tmp`, fsync, rename over the
       target (per-file atomicity: a file is never observed truncated);
     - `remove`: delete only if the current hash matches what we deployed;
       restore its `backup` if one exists;
   * on any error: **rollback** — walk done journal rows in reverse, undo each
     (delete written files, restore moved backups), mark deployment
     `rolled_back`;
   * on success: update `deployed_files` to the new state and mark the
     deployment `committed` in **one** SQLite transaction. DB state therefore
     only ever reflects fully-applied deployments.
5. **Crash recovery**: a `running` deployment found at startup blocks further
   deploy/purge; `lmm rollback` replays the journal in reverse, verifying each
   step against the filesystem (idempotent — safe to re-run after a second
   crash), then marks it `rolled_back`.

`purge` is the same machinery with an empty desired state: every deployed file
is removed and every backup restored.

### Filesystem safety invariants

* All target paths are `RelPath`s: relative, normalized, no `..`, no absolute
  components, `/` separators, sane component lengths.
* Before writing under a target directory, its canonicalized path must still
  be inside the canonicalized game root — protects against symlinked
  subdirectories pointing out of the game tree.
* lmm never deletes or overwrites a file whose content it cannot account for
  (hash matches a deployed file or a recorded backup). Unaccounted files are
  reported and skipped unless `--force`.
* Backups are moved, never copied-then-deleted, and never overwritten.

---

## 6. Rollback strategy

Three layers:

1. **Per-file atomicity** — temp file + rename; a crash mid-copy leaves only a
   `.lmm-tmp` file, never a corrupt target.
2. **Journal rollback** — every fs op is journaled before it runs, with enough
   information (`pre_sha256`, `new_sha256`, `backup_id`) to undo it and to
   *verify* the undo is operating on the expected bytes. Failed deployments
   roll back automatically; interrupted ones roll back via `lmm rollback`.
3. **Purge** — full teardown to the pre-lmm state: remove all deployed files,
   restore all backups. Always available as the escape hatch.

"Unwanted" (but successful) changes are reverted by disabling mods and
re-deploying, or by purging — no deployment history snapshots in v1.

---

## 7. Game-discovery strategy

No filesystem-wide scanning. Candidate Steam roots, checked for existence:

```
~/.local/share/Steam
~/.steam/steam, ~/.steam/root           (usually symlinks to the above)
~/.var/app/com.valvesoftware.Steam/data/Steam   (Flatpak)
+ extra roots from config.toml
```

For each root: parse `steamapps/libraryfolders.vdf` (VDF text format, minimal
hand-rolled parser) → the set of library paths. For each library, parse
`steamapps/appmanifest_<appid>.acf` → app id, name, install dir
(`steamapps/common/<installdir>`). Proton prefix, if present:
`steamapps/compatdata/<appid>/pfx`.

Discovered apps are matched against the built-in game registry by Steam app
id. `lmm scan` reports supported games (and, with `--all`, everything found);
`lmm game add` registers an installation, either from a scan result or from a
manual `--path` (with `--game generic` for unknown games — mods deploy to the
game root or a `--mod-dir` subdirectory).

First supported game: **Skyrim Special Edition** (app id 489830) — the
best-understood modding target. Mods target the `Data/` subdirectory. Layout
detection for its archives:

* archive root contains `Data/` → strip it;
* archive root looks like Data content (`*.esp/esm/esl/bsa/ba2`, `meshes/`,
  `textures/`, `scripts/`, ...) → treat root as `Data/`;
* single wrapper directory → descend and retry;
* otherwise → install as-is relative to `Data/` with a warning.

The registry is a plain table of `GameDef { slug, name, steam_app_id,
mod_root: &str, layout: LayoutRules }` — adding a game is adding a row plus
optional layout rules.

---

## 8. Archive handling & threat model

Formats: ZIP (`zip` crate) and 7z (`sevenz-rust2`). RAR is detected and
rejected with a helpful message (candidate for later via libunrar).

Import pipeline: enumerate → validate → extract to `tmp/` → detect layout →
hash inventory → move into `staging/<mod_id>/` → DB commit. The game directory
is never an extraction target.

Threats and mitigations (archives and paths are untrusted input):

| Threat | Mitigation |
|---|---|
| Path traversal (`../`, `..\`) | every entry parsed into `RelPath`; any `..`/`.`/empty component ⇒ reject archive |
| Absolute paths in entries | `RelPath` rejects leading `/`, drive letters, UNC prefixes |
| Symlinks/hardlinks in archives | entries that are not plain files/dirs are rejected |
| Symlink escape at deploy time | canonical-prefix check of every target directory against the game root |
| Decompression bombs | per-file size cap, total-size cap, entry-count cap, compression-ratio guard (all configurable); extraction streams with a hard byte budget, not trusting header sizes |
| Accidental deletion of unmanaged files | delete/overwrite only when content hash matches recorded state; otherwise skip + report, `--force` to override |
| Partial deployment | write-ahead journal + automatic reverse rollback; per-file temp+rename |
| DB/filesystem divergence | `deployed_files` updated only on full commit; `verify` detects drift, `repair` re-copies from staging or adopts, `rollback` recovers interrupted runs |

---

## 9. CLI command plan

```
lmm scan [--all]                          discover Steam games
lmm game add <path>|--app <scan-result>   register installation (manual or from scan)
lmm game list                             registered installations
lmm game use <id>                         set default installation
lmm game remove <id>                      unregister (refuses while deployed)

lmm install <archive> [--name N --version V]
lmm mods [list]                           installed mods, order, enabled, conflicts summary
lmm enable <mod>… / disable <mod>…
lmm order <mod> <position>                change priority (1 = lowest/loses)
lmm uninstall <mod>                       remove mod (refuses/redeploys if deployed)

lmm conflicts [--path <glob>]             conflicting files, winners and losers
lmm deploy [--dry-run] [--force]
lmm purge  [--dry-run] [--force]
lmm verify                                fs vs db drift report
lmm repair [--dry-run] [--force]          fix drift from staging/backups
lmm rollback                              undo interrupted/failed deployment

lmm profile list/create/switch/delete/copy
lmm status                                installation, profile, deployment summary
lmm launch                                start the game (steam://rungameid/<id>)
```

Global flags: `--json` (stable machine-readable output on stdout, diagnostics
on stderr), `--verbose`, `--yes` (non-interactive), `--config <path>`,
`--db <path>`, `--data-dir <path>`, `--game <installation>` where relevant.
Exit codes: 0 ok, 1 operational error, 2 usage error.

Mods are addressable by numeric id or unambiguous name (case-insensitive).

---

## 10. Incremental implementation plan

1. Workspace, config, XDG paths, SQLite schema + migrations, `status`.
2. VDF parser, Steam discovery, game registry, `scan` / `game add|list|use`.
3. `RelPath`, archive validation + bounded extraction (ZIP, 7z), staging.
4. Mod install + inventory (`install`, `mods`, `uninstall` pre-deployment).
5. Resolution + `conflicts`; profiles (`profile *`, enable/disable/order).
6. Deployment plan + execute + journal + backups (`deploy`, `purge`, dry-run).
7. `verify`, `repair`, `rollback`; crash-recovery gate.
8. `launch`; polish: JSON everywhere, error messages, README.
9. Interactive shell (primary interface) + Nexus API + nxm:// handler with
   download queue (docs/SHELL.md, docs/NEXUS.md).
10. FOMOD installers (docs/FOMOD.md).
11. Hardlink deployment (`[deploy] method`); expanded game registry
    (Bethesda family incl. Morrowind's `Data Files/`, plus `ModFolder`
    layout games: Stardew Valley, Bannerlord, 7 Days to Die; Cyberpunk
    2077 as game-root).
12. Game Tools: per-game tool catalogs with managed installs (manifest +
    backups), guided setup, INI configuration tweaks, plugins.txt
    analysis/sorting, health check (docs/TOOLS.md; schema v4).
13. (later) more games, TUI.

Each stage ends with: `cargo build` clean, `cargo fmt`, `cargo clippy` clean,
`cargo test` green.

Testing uses tempdir game roots + tempdir data dirs and archives generated in
the test. Key scenarios: two mods same file; priority flip; disable winner →
next provider restored; vanilla file backup + restore on purge; injected
mid-deploy failure → rollback leaves pristine tree; external edit detected by
verify and refused by deploy; traversal/absolute/symlink archives rejected;
bomb caps enforced; missing staged file; moved game dir; db/fs mismatch repair.