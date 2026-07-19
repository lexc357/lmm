# FOMOD installer support

FOMOD is the XML-based installer format used by most large Bethesda-game mods
(`fomod/ModuleConfig.xml` inside the archive). lmm runs the installer in the
terminal, turns the user's choices into a validated file plan, and hands that
plan to the exact same staging/deployment machinery a plain archive uses.

## Integration plan

The existing import pipeline is
`hash → extract to scratch → detect layout → inventory → rename into staging`
(`staging::import_archive`), orchestrated by `mods::install`, called from
`cmd::mods::install` (which both the CLI/shell `install` command and
`downloads install` go through).

FOMOD support splits that pipeline at the point where a human is needed:

```text
cmd::mods::install (lmm-cli — the only place that prompts)
  └─ mods::prepare_install (core: name + duplicate checks, hash, extract)
       ├─ no fomod/ModuleConfig.xml → mods::finish_plain
       │    (detect layout → inventory → stage → record; unchanged behavior)
       └─ fomod detected (and not --manual)
            ├─ fomod::parse       ModuleConfig.xml → Module (pure data)
            ├─ fomod::Session     pure selection state machine (core)
            │    driven by cmd::fomod::run_installer (terminal UI, cli)
            ├─ fomod::plan::build selections → validated InstallPlan
            └─ mods::finish_fomod materialize plan into staging → record
                                  + persist choices/plan (fomod_installs)
```

Where each concern lives:

* **Detection** — `fomod::detect` walks the extracted tree (bounded depth)
  for a `fomod/ModuleConfig.xml`, case-insensitively. The installer root is
  the directory containing `fomod/`.
* **Parsing** — `fomod::parse` (quick-xml, streaming, hard limits). Produces
  `fomod::Module`, plain data, order-preserving. No I/O beyond the two XML
  files, no environment access.
* **Condition evaluation** — `fomod::cond`. Tri-state logic
  (`True/False/Unknown{reason}`) over a `fomod::Environment` trait; the game
  adapter implementation (`fomod::env::InstallEnvironment`) answers file /
  plugin / version questions from the installation directory and the mod
  database. Parsing never sees the environment; the UI never sees XML.
* **Selection session** — `fomod::Session`: visible steps, group rules,
  plugin types, flags, auto-selection, explanations ("why is this locked").
  Pure state; the terminal UI in `cmd::fomod` only renders it and forwards
  commands. Nothing is written anywhere while the session runs.
* **Plan** — `fomod::plan`: required files + selected plugins' files +
  conditional installs, folders expanded, both sides validated as `RelPath`,
  sources resolved case-insensitively against the archive inventory,
  duplicate destinations resolved by FOMOD priority (ties on identical
  content are silent; real ambiguity is surfaced and needs confirmation).
* **Staging** — `staging::import_fomod_plan` copies the planned files into a
  fresh directory and promotes it exactly like a plain import. From here on
  (enable/disable/conflicts/deploy/verify/repair/rollback/purge) a FOMOD mod
  is indistinguishable from any other mod.
* **Persistence** — migration v3 adds `fomod_installs` (one row per mod):
  module name, ModuleConfig hash, selected step/group/option identifiers,
  final flags, and the normalized plan, all reproducible JSON. The archive
  hash already lives on `mods`.
* **Reconfigure / reinstall** — `fomod reconfigure` re-runs the installer
  with saved choices preselected (where still valid), diffs the new plan
  against the stored one, asks for confirmation, and swaps staging + file
  inventory in one transaction. Deployed mods must be disabled + deployed
  (or purged) first, mirroring the uninstall rule, so a failed reconfigure
  can never corrupt the game directory.

## Supported features

* `moduleName`, `moduleImage`, `info.xml` metadata (name/author/version).
* `requiredInstallFiles`, `installSteps`/`optionalFileGroups`/`plugins`
  with explicit or alphabetic (`Ascending`/`Descending`) ordering;
  document order is preserved for `Explicit`.
* Group types: `SelectExactlyOne`, `SelectAtMostOne`, `SelectAtLeastOne`,
  `SelectAny`, `SelectAll`.
* Plugin types: `Required`, `Recommended`, `Optional`, `NotUsable`,
  `CouldBeUsable`, and `dependencyType` (default + condition patterns).
* `conditionFlags` (flags set by selected options; later steps re-evaluate).
* Step `visible` conditions.
* `conditionalFileInstalls` patterns evaluated against the final flag set.
* Dependencies: `fileDependency` (state `Active`/`Inactive`/`Missing` —
  `Missing` is FOMOD's negation), `flagDependency`, `gameDependency`,
  `foseDependency` (script extender), nested `<dependencies>` with
  `And`/`Or` operators.
* `file`/`folder` mappings with `source`, `destination`, `priority`;
  a missing `destination` mirrors the source path (files) or the mod root
  (folders). A leading `Data/` on destinations is stripped for
  Bethesda-layout games, since staging is already Data-relative.

## Unsupported (by design, reported when encountered)

* C#/script-based installers (`fomod/script.cs`, ancient OMOD conversions):
  lmm never executes code shipped in a mod archive.
* `installIfUsable`/`alwaysInstall` plugin-file attributes are parsed and
  honored in their common meaning (install regardless of selection when the
  type allows); exotic combinations fall back to a warning.
* Remote resources of any kind. XML external entities are not expanded
  (quick-xml does not resolve them; lmm additionally caps size and depth).
* `fommDependency` versions; game/script-extender *versions* on Linux are
  usually unknowable — see limitations.

## Linux / Proton limitations

`gameDependency`/`foseDependency` version checks cannot be answered reliably
under Proton (no Windows version resources). The evaluator returns
`Unknown` with a reason instead of guessing. Interactively, lmm shows the
reason and asks whether to treat the condition as satisfied; in
non-interactive runs an Unknown that would change installed files is a hard
error. Script-extender *presence* is checked via its loader executable in
the game directory.

## Selection rules, as enforced

* `Required` options are selected and cannot be unselected.
* `NotUsable` options cannot be selected; the UI explains why (which
  dependency failed).
* `Recommended` options are preselected but can be toggled.
* Group rules are enforced on `next`, with exact messages
  ("this group needs at least one selection").
* `SelectExactlyOne`/`SelectAtMostOne` behave like radio buttons: picking an
  option replaces the previous pick.
* Flags from selections apply in step order; hiding a step drops its flags
  and selections from consideration (matching mainstream manager behavior).

## Security restrictions

* All archive/XML content is untrusted. Paths on both sides of every
  mapping go through `RelPath::parse` (rejects absolute paths, traversal,
  drive letters, control characters, over-long components).
* XML: 8 MiB size cap, 64-level depth cap, and counted caps on steps,
  groups, options, flags and file mappings (see `fomod::limits`).
* Images are never opened automatically; `image <n>` prints the path inside
  the extracted archive, and only a viewer the *user* configured
  (`[fomod] image_viewer` in config.toml) is ever spawned — never anything
  from the archive.
* The installer session performs no filesystem writes; the plan is built
  and validated only after choices are complete.

## Reconfiguration behavior

`fomod choices <mod>` shows the stored selections. `fomod reinstall <mod>`
replays the stored plan against the original archive (located via the
download store or `--archive`); `fomod reconfigure <mod>` reopens the
installer preseeded with stored choices. Both verify the archive hash and
the ModuleConfig hash: if either changed, saved choices are only used where
they still resolve, and the user is told. The new plan is diffed against
the installed one (added / removed / replaced files) and applied in a
single transaction after confirmation; on any failure the previous
installation stays intact.
