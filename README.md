# lmm — Linux Mod Manager

A Linux-native CLI/shell mod manager for games, built for Steam/Proton
installations and Nexus Mods archives. First-class support for the Bethesda
family (Skyrim SE/LE/VR, Oblivion, Morrowind, Fallout 3/NV/4, Starfield,
Enderal SE), Stardew Valley, Cyberpunk 2077, Mount & Blade II: Bannerlord,
and 7 Days to Die; any other game can be managed as `generic`.

lmm is safe by construction: mods are staged outside the game directory,
every deployment is journaled and can be rolled back, originals are backed up
before they are overwritten, and lmm never deletes or overwrites a file whose
content it cannot account for. `purge` always restores the exact pre-lmm
state.

---

## Quick install

Requirements: Linux, a [Rust toolchain](https://rustup.rs) (edition 2024, so
Rust 1.85+). Archive extraction (`.zip`, `.7z`) is built in — no external
tools needed.

```sh
git clone <repository-url> && cd lmm
cargo install --path crates/lmm-cli
```

Then get going in under a minute:

```
$ lmm
Linux Mod Manager v0.1.0 — 'help' lists commands, 'q' quits
lmm> scan                        # find your Steam games (native, Proton, Flatpak)
lmm> game add --app 489830       # register one (Skyrim SE shown)
lmm [skyrimse]> install ~/Downloads/SkyUI_5_2_SE-12604-5-2SE.7z
lmm [skyrimse]> enable skyui
lmm [skyrimse]> deploy           # copy files in — originals backed up
lmm [skyrimse]> launch
```

Running `lmm` with no arguments opens the interactive shell (history, line
editing, tab completion, current game/profile in the prompt). Every command
also works as a one-shot invocation (`lmm deploy --dry-run`) — same parser,
same behavior — so everything scripts.

---

## Popular use cases

### Mod a Bethesda game from zero

The `tools setup` wizard installs the community-standard toolkit (SKSE,
LOOT, xEdit, …), applies the INI settings modding requires (with backups),
and sorts your load order — every step skippable:

```
lmm [skyrimse]> tools setup          # guided first-time setup
lmm [skyrimse]> tools check          # "is this game ready for modding?" checklist
lmm [skyrimse]> install ~/Downloads/unofficial-patch.7z
lmm [skyrimse]> enable unofficial-patch
lmm [skyrimse]> deploy
```

### One-click downloads from Nexus Mods

Make lmm the handler for the "Mod Manager Download" button:

```
lmm> nexus apikey               # prompts for your key from the Nexus site
lmm> nexus register             # register lmm for nxm:// links (XDG)
  ... click "Mod Manager Download" in the browser ...
nxm: queued 'SkyUI' (SkyUI_5_2_SE-12604-5-2SE.7z) as download 1
lmm [skyrimse]> downloads start 1
lmm [skyrimse]> downloads install 1
```

Links arrive while the shell runs (a background notification — downloads
never block the prompt) or are stored and picked up at the next start.
Nothing is downloaded without an explicit `downloads start`.

### Install a mod with a FOMOD installer

Most large Bethesda mods ship an XML installer. lmm detects it and runs it
in the terminal — pick your options, and the result goes through the same
staging/deploy pipeline as a plain archive:

```
lmm [skyrimse]> install ~/Downloads/big-texture-overhaul.7z
  ... installer pages: choose options, Enter to continue ...
lmm [skyrimse]> enable big-texture-overhaul
lmm [skyrimse]> deploy
```

Pass `--manual` to skip the installer and stage the archive as-is.

### See conflicts and control who wins

```
lmm [skyrimse]> conflicts            # files claimed by >1 enabled mod, with winners
lmm [skyrimse]> order skyui 3        # move a mod; later in the order wins
lmm [skyrimse]> deploy --dry-run     # see exactly what would change on disk
lmm [skyrimse]> deploy
```

### Try a different setup without losing your current one

Profiles are named mod configurations (enabled state + load order) per
installation:

```
lmm [skyrimse]> profile create survival
lmm [skyrimse]> profile switch survival
  ... enable/disable a different set ...
lmm [skyrimse]> deploy               # game directory now matches this profile
lmm [skyrimse]> profile switch default
```

### Undo everything

```
lmm [skyrimse]> purge                # remove all deployed files, restore originals
```

The game directory is back to its exact pre-lmm state. Your staged mods and
profiles are untouched — `deploy` brings them back.

### Script it

Every command takes `--json` (one machine-readable document on stdout,
diagnostics on stderr) and `--yes` (non-interactive):

```sh
lmm --json --game skyrimse conflicts | jq '.conflicts[].path'
lmm --yes deploy
```

Exit codes: 0 ok, 1 operational error, 2 usage error.

---

## Detailed guide

### The shell

`lmm` with no arguments opens the interactive shell — the primary interface.
It has history, line editing, tab completion, and shows the current game and
profile in the prompt. Everything the shell can do, the one-shot CLI can do
too. See [docs/SHELL.md](docs/SHELL.md).

### Registering games

`scan` discovers Steam games across native, Proton, and Flatpak Steam
installs; `game add --app <steam-appid>` registers one from the scan, and
`game add <path>` registers any directory manually (use game type `generic`
for unsupported games). `game list`, `game use`, and `game remove` manage
the registry; `use <install>` is a shorthand for `game use`. `--game
<install>` on any command targets a specific installation without switching.

### Installing mods

`install <archive>` validates the archive (path traversal, symlinks,
archive bombs), extracts it to `~/.local/share/lmm/staging/`, detects the
mod's layout, and records an inventory of every file. The game directory is
never an extraction target. Archives with a FOMOD installer
(`fomod/ModuleConfig.xml`) launch the terminal installer described above;
`--manual` bypasses it. See [docs/FOMOD.md](docs/FOMOD.md).

`mods` lists installed mods in load order; `enable`/`disable` toggle them in
the active profile; `uninstall` removes a mod entirely.

### Profiles, load order, conflicts

Each profile stores enabled state and load order. Conflicts resolve
case-insensitively (Proton games do); the mod later in the load order wins.
`conflicts` shows every contested file and its winner; `order <mod> <pos>`
rearranges. Changing mods or profiles only changes the *desired* state —
nothing touches the game directory until `deploy`.

### Deployment

`deploy` diffs the desired state against what lmm previously deployed and
copies only the difference. Every filesystem operation is journaled in
SQLite before it runs; each file is written via temp + fsync + rename. On
any failure the journal is replayed in reverse, leaving the game directory
exactly as it was — `rollback` does the same for an interrupted run.

Before a mod file takes an original's place, the original is *moved* (never
copied-and-deleted) into `~/.local/share/lmm/backups/`, and moved back on
`purge` or when no mod provides that path anymore.

With `[deploy] method = "hardlink"` in config.toml, files are hard-linked
from staging instead of copied — instant, no duplicated disk space — with
automatic per-file fallback to copying when the game and staging live on
different filesystems. Caveat: a tool that rewrites a deployed file *in
place* also rewrites the staged copy; `verify` flags this, but repair then
needs the mod reinstalled.

### Verify and repair

Files modified outside lmm are detected by hash and never overwritten or
deleted without `--force`. `verify` re-hashes everything and reports
database↔filesystem drift; `repair` re-copies from staging and can even
rebuild lost staging files from intact deployed copies.

### Nexus Mods integration

`nexus apikey` stores your personal API key, `nexus register` makes lmm the
XDG handler for `nxm://` links, `nexus status` shows account state. Queued
links become numbered downloads managed with `downloads`
(`start`/`cancel`/`retry`/`remove`) and installed with `downloads install
<id>` — the exact same validation/staging pipeline as local files. See
[docs/NEXUS.md](docs/NEXUS.md).

### Game Tools

`lmm tools` covers everything a game needs *besides* mods: script extenders,
LOOT/xEdit-style utilities, required INI settings, and plugin load order.
lmm knows the community-standard toolkit for each supported game and can
install, verify, update, launch, and remove tools; apply the configuration
changes modding requires (with automatic backups and `restore`); sort
`plugins.txt`; and run a readiness checklist:

```
lmm [skyrimse]> tools                # SKSE, Address Library, LOOT, ... with status
lmm [skyrimse]> tools install skse ~/Downloads/skse64_2_02_06.7z
lmm [skyrimse]> tools config show    # modding-required INI settings
lmm [skyrimse]> tools loadorder sort
```

See [docs/TOOLS.md](docs/TOOLS.md).

### Command reference

| Command | What it does |
|---|---|
| `lmm scan [--all]` | discover Steam games (native, Proton, Flatpak Steam) |
| `lmm game add <path>\|--app <id>` | register an installation (manual path or scan result) |
| `lmm game list / use / remove` | manage registered installations |
| `lmm install <archive> [--manual]` | stage a mod from a `.zip`/`.7z` archive (FOMOD-aware) |
| `lmm mods` | installed mods in load order |
| `lmm enable / disable <mod>…` | toggle mods in the active profile |
| `lmm order <mod> <pos>` | move a mod in the load order (higher wins conflicts) |
| `lmm conflicts` | files claimed by more than one enabled mod, with winners |
| `lmm uninstall <mod>` | remove a mod entirely |
| `lmm deploy [--dry-run] [--force]` | reconcile the game directory with the active profile |
| `lmm purge [--dry-run] [--force]` | remove all deployed files, restore originals |
| `lmm verify` | re-hash everything and report database↔filesystem drift |
| `lmm repair [--dry-run] [--force]` | fix drift from staging/backups |
| `lmm rollback` | undo an interrupted or failed deployment |
| `lmm profile list/create/switch/delete/copy` | named mod configurations per installation |
| `lmm status` | installation, profile, and deployment summary |
| `lmm launch` | start the game via Steam |
| `lmm use <install>` | set the default installation (shorthand for `game use`) |
| `lmm nexus apikey/register/status` | Nexus account setup and nxm:// handler registration |
| `lmm nxm <url>` | handle an nxm:// link (what the browser handler invokes) |
| `lmm downloads` | list Nexus downloads (pending/active/completed/failed) |
| `lmm downloads start/cancel/retry/remove <id>` | manage queued downloads |
| `lmm downloads install <id>` | install a completed download (normal pipeline) |
| `lmm tools` | Game Tools: the game's essential modding utilities and their status |
| `lmm tools install/verify/launch/remove <tool>` | manage tools (SKSE, LOOT, xEdit, …) with full manifests and backups |
| `lmm tools setup` | guided first-time setup: essential tools, configuration, load order |
| `lmm tools check` | "is this game ready for modding?" checklist |
| `lmm tools config show/apply/restore` | modding-required INI settings (archive invalidation etc.) |
| `lmm tools loadorder [sort/backups/restore]` | analyze and sort plugins.txt, with automatic backups |

Global flags: `--json`, `--yes`, `--game <install>`, `--verbose`,
`--config/--data-dir/--db <path>`.

### Files and configuration

Data lives in `~/.local/share/lmm/` (`staging/`, `backups/`, the SQLite
database; override with `--data-dir`), config in
`~/.config/lmm/config.toml`. See [docs/DESIGN.md](docs/DESIGN.md) for the
full design.

## Development

```sh
cargo build && cargo test && cargo clippy --all-targets
```

Three crates: `lmm-core` (all logic, no user I/O), `lmm-nexus` (Nexus API
client, nxm:// links, download queue), and `lmm-cli` (thin argument/output
layer). Anything destructive is split into a pure *plan* step and an
*execute* step that takes the plan — dry-run is printing the plan.

## License

MIT OR Apache-2.0
