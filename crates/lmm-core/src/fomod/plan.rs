//! Turning completed FOMOD selections into a validated installation plan.
//!
//! The plan is the security boundary between installer data and the
//! filesystem: every source and destination goes through [`RelPath`], all
//! sources must exist inside the extracted archive (resolved
//! case-insensitively — XML authors rarely match their archive's casing),
//! and destination collisions are resolved by FOMOD priority rules or
//! surfaced for confirmation. Nothing is copied here; the staging layer
//! executes the finished plan.
//!
//! Collision semantics (matching mainstream managers):
//! * higher `priority` wins;
//! * equal priority: the mapping installed later (document/step order)
//!   wins — a defined FOMOD ordering, so it resolves silently when both
//!   sides came from the *same* option or one of them is required files;
//! * equal priority across *different options* with *different content* is
//!   ambiguous: recorded in [`InstallPlan::ambiguities`] and the frontend
//!   must confirm before installing;
//! * identical content (same hash) is never a conflict.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, IoContext, Result};
use crate::hash::sha256_file;
use crate::paths::RelPath;

use super::cond::{self, Environment, Eval, Flags};
use super::model::{Mapping, Module, OptionType, TypeDescriptor};
use super::session::Selections;

/// One file to install: copy `source` (relative to the installer root)
/// to `dest` (relative to the game's mod root, i.e. the staging root).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlannedFile {
    pub source: RelPath,
    pub dest: RelPath,
    /// Which part of the installer supplied this file (for provenance:
    /// "required files", "step 'X' > group 'Y' > option 'Z'", …).
    pub origin: String,
    pub priority: i64,
}

/// A destination claimed by two mappings with no clearly-defined winner.
#[derive(Debug, Clone, Serialize)]
pub struct Ambiguity {
    pub dest: String,
    /// Origin that wins if the user proceeds (later install order).
    pub winner: String,
    pub loser: String,
}

/// A conditional-install pattern whose condition cannot be evaluated on
/// this machine. The frontend must decide (ask the user / refuse) and
/// rebuild with an entry in `resolutions`.
#[derive(Debug, Clone)]
pub struct Unresolved {
    /// Index into `module.conditional_installs`.
    pub index: usize,
    pub condition: String,
    pub reason: String,
    pub file_count: usize,
}

#[derive(Debug, Default, Serialize)]
pub struct InstallPlan {
    /// Final mapping, one entry per destination, sorted by destination.
    pub files: Vec<PlannedFile>,
    /// Same-priority, cross-option collisions with differing content.
    /// Installing without confirming these is not allowed.
    pub ambiguities: Vec<Ambiguity>,
    /// Resolved overlaps and other non-blocking notes.
    pub notes: Vec<String>,
}

/// Outcome of a build: either the plan is complete, or conditions listed in
/// `unresolved` need answers first (pass them via `resolutions`).
#[derive(Debug)]
pub struct BuildOutcome {
    pub plan: InstallPlan,
    pub unresolved: Vec<Unresolved>,
}

/// Options controlling destination fix-ups.
#[derive(Debug, Clone, Copy, Default)]
pub struct DestRules {
    /// Strip one leading `Data/` from destinations (Bethesda-layout games,
    /// where staging is already Data-relative).
    pub strip_data_prefix: bool,
}

pub fn build(
    module: &Module,
    selections: &Selections,
    installer_root: &Path,
    env: &dyn Environment,
    rules: DestRules,
    resolutions: &BTreeMap<usize, bool>,
) -> Result<BuildOutcome> {
    let index = ArchiveIndex::scan(installer_root)?;
    let mut b = Builder {
        index,
        rules,
        installer_root: installer_root.to_path_buf(),
        entries: Vec::new(),
        notes: Vec::new(),
        seq: 0,
    };

    for m in &module.required_files {
        b.add_mapping(m, "required files")?;
    }

    // Selected options in module order; also honor alwaysInstall /
    // installIfUsable on options that were *not* selected.
    let selected = selection_set(selections);
    for sc in &selections.steps {
        let step = &module.steps[sc.step_index];
        for (gi, group) in step.groups.iter().enumerate() {
            for (oi, opt) in group.options.iter().enumerate() {
                let origin = format!(
                    "step '{}' > group '{}' > option '{}'",
                    step.name, group.name, opt.name
                );
                if selected.contains(&(sc.step_index, gi, oi)) {
                    for m in &opt.files {
                        b.add_mapping(m, &origin)?;
                    }
                    continue;
                }
                // Unselected: only the legacy per-file attributes apply.
                for m in &opt.files {
                    if m.always_install {
                        b.add_mapping(m, &format!("{origin} [alwaysInstall]"))?;
                    } else if m.install_if_usable
                        && resolved_type(&opt.type_desc, &selections.flags, env)
                            != OptionType::NotUsable
                    {
                        b.add_mapping(m, &format!("{origin} [installIfUsable]"))?;
                    }
                }
            }
        }
    }

    // Conditional installs against the final flag set.
    let mut unresolved = Vec::new();
    for (i, ci) in module.conditional_installs.iter().enumerate() {
        let include = match cond::eval_composite(&ci.when, &selections.flags, env) {
            Eval::True => true,
            Eval::False => false,
            Eval::Unknown(reason) => match resolutions.get(&i) {
                Some(&answer) => answer,
                None => {
                    unresolved.push(Unresolved {
                        index: i,
                        condition: cond::describe_composite(&ci.when),
                        reason,
                        file_count: ci.files.len(),
                    });
                    continue;
                }
            },
        };
        if include {
            for m in &ci.files {
                b.add_mapping(
                    m,
                    &format!(
                        "conditional install ({})",
                        cond::describe_composite(&ci.when)
                    ),
                )?;
            }
        }
    }

    let plan = b.resolve()?;
    Ok(BuildOutcome { plan, unresolved })
}

fn selection_set(s: &Selections) -> std::collections::BTreeSet<(usize, usize, usize)> {
    s.steps
        .iter()
        .flat_map(|sc| {
            sc.groups.iter().flat_map(move |gc| {
                gc.options
                    .iter()
                    .map(move |oc| (sc.step_index, gc.group_index, oc.option_index))
            })
        })
        .collect()
}

fn resolved_type(td: &TypeDescriptor, flags: &Flags, env: &dyn Environment) -> OptionType {
    match td {
        TypeDescriptor::Simple(t) => *t,
        TypeDescriptor::Dependent { default, patterns } => {
            for p in patterns {
                if cond::eval_composite(&p.when, flags, env) == Eval::True {
                    return p.becomes;
                }
            }
            *default
        }
    }
}

/// Case-insensitive inventory of the extracted archive, so XML `source`
/// paths resolve regardless of casing mismatches.
struct ArchiveIndex {
    /// lowercase rel path -> actual rel path (files only).
    files: HashMap<String, RelPath>,
    /// lowercase rel path of every directory.
    dirs: std::collections::HashSet<String>,
}

impl ArchiveIndex {
    fn scan(root: &Path) -> Result<ArchiveIndex> {
        let mut files = HashMap::new();
        let mut dirs = std::collections::HashSet::new();
        for entry in walkdir::WalkDir::new(root).follow_links(false) {
            let entry =
                entry.map_err(|e| Error::Invalid(format!("walking {}: {e}", root.display())))?;
            if entry.path() == root {
                continue;
            }
            let rel_os = entry
                .path()
                .strip_prefix(root)
                .map_err(|_| Error::Invalid("walkdir escaped root".into()))?;
            let rel = RelPath::from_os_rel(rel_os)?;
            if entry.file_type().is_dir() {
                dirs.insert(rel.key());
            } else if entry.file_type().is_file() {
                files.insert(rel.key(), rel);
            }
        }
        Ok(ArchiveIndex { files, dirs })
    }
}

/// One planned entry before collision resolution.
struct Entry {
    file: PlannedFile,
    /// Install order: later entries beat earlier ones at equal priority.
    seq: usize,
}

struct Builder {
    index: ArchiveIndex,
    rules: DestRules,
    installer_root: PathBuf,
    entries: Vec<Entry>,
    notes: Vec<String>,
    seq: usize,
}

impl Builder {
    /// Validate and expand one XML mapping into planned files.
    fn add_mapping(&mut self, m: &Mapping, origin: &str) -> Result<()> {
        self.seq += 1;
        let seq = self.seq;
        if m.is_folder {
            self.add_folder(m, origin, seq)
        } else {
            self.add_file(m, origin, seq)
        }
    }

    fn add_file(&mut self, m: &Mapping, origin: &str, seq: usize) -> Result<()> {
        let source = RelPath::parse(&m.source)
            .map_err(|e| Error::Fomod(format!("{origin}: invalid source path: {e}")))?;
        let Some(actual) = self.index.files.get(&source.key()).cloned() else {
            if self.index.dirs.contains(&source.key()) {
                return Err(Error::Fomod(format!(
                    "{origin}: source '{}' is a directory (use <folder>)",
                    m.source
                )));
            }
            return Err(Error::Fomod(format!(
                "{origin}: source file '{}' does not exist in the archive",
                m.source
            )));
        };
        // Destination: omitted mirrors the source path; explicitly empty
        // means "the file's name at the mod root".
        let dest_raw = match m.dest.as_deref() {
            None => m.source.clone(),
            Some("") => file_name_of(&actual),
            Some(d) => d.to_string(),
        };
        let dest = self.parse_dest(&dest_raw, origin)?;
        self.entries.push(Entry {
            file: PlannedFile {
                source: actual,
                dest,
                origin: origin.to_string(),
                priority: m.priority,
            },
            seq,
        });
        Ok(())
    }

    fn add_folder(&mut self, m: &Mapping, origin: &str, seq: usize) -> Result<()> {
        // An empty folder source means the installer root itself; the
        // fomod/ metadata directory is never game content, so it is
        // excluded from that expansion.
        let (prefix_key, root_expansion) = if m.source.trim().is_empty() {
            (String::new(), true)
        } else {
            let source = RelPath::parse(&m.source)
                .map_err(|e| Error::Fomod(format!("{origin}: invalid source path: {e}")))?;
            if !self.index.dirs.contains(&source.key()) {
                if self.index.files.contains_key(&source.key()) {
                    return Err(Error::Fomod(format!(
                        "{origin}: source '{}' is a file (use <file>)",
                        m.source
                    )));
                }
                return Err(Error::Fomod(format!(
                    "{origin}: source folder '{}' does not exist in the archive",
                    m.source
                )));
            }
            (format!("{}/", source.key()), false)
        };

        let dest_prefix = match m.dest.as_deref() {
            None | Some("") => String::new(),
            Some(d) => format!("{d}/"),
        };

        let mut found = 0usize;
        // Deterministic expansion order: sort by key.
        let mut matches: Vec<(&String, &RelPath)> = self
            .index
            .files
            .iter()
            .filter(|(k, _)| k.starts_with(&prefix_key))
            .collect();
        matches.sort_by_key(|(k, _)| (*k).clone());
        for (_key, actual) in matches {
            if root_expansion && actual.key().starts_with("fomod/") {
                continue;
            }
            // Remainder keeps the archive's on-disk casing.
            let remainder = &actual.as_str()[prefix_key.len()..];
            let dest = self.parse_dest(&format!("{dest_prefix}{remainder}"), origin)?;
            self.entries.push(Entry {
                file: PlannedFile {
                    source: actual.clone(),
                    dest,
                    origin: origin.to_string(),
                    priority: m.priority,
                },
                seq,
            });
            found += 1;
        }
        if found == 0 {
            self.notes
                .push(format!("{origin}: folder '{}' contains no files", m.source));
        }
        Ok(())
    }

    fn parse_dest(&self, raw: &str, origin: &str) -> Result<RelPath> {
        let dest = RelPath::parse(raw)
            .map_err(|e| Error::Fomod(format!("{origin}: invalid destination path: {e}")))?;
        if self.rules.strip_data_prefix {
            let mut comps = dest.components();
            if comps.next().is_some_and(|c| c.eq_ignore_ascii_case("data")) {
                let rest: Vec<&str> = comps.collect();
                if rest.is_empty() {
                    return Err(Error::Fomod(format!(
                        "{origin}: destination '{raw}' points at the Data directory itself"
                    )));
                }
                return RelPath::parse(&rest.join("/"));
            }
        }
        Ok(dest)
    }

    /// Collapse entries into one file per destination, applying the
    /// priority/order rules from the module docs above.
    fn resolve(self) -> Result<InstallPlan> {
        let Builder {
            entries,
            mut notes,
            installer_root,
            ..
        } = self;
        // Group by destination key, keeping arrival order.
        let mut by_dest: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
        for e in entries {
            by_dest.entry(e.file.dest.key()).or_default().push(e);
        }

        let mut files = Vec::new();
        let mut ambiguities = Vec::new();
        for (_key, mut claims) in by_dest {
            // Winner: highest priority, then latest install order.
            claims.sort_by_key(|e| (e.file.priority, e.seq));
            let winner = claims.pop().expect("group is non-empty");
            for loser in &claims {
                if loser.file.source == winner.file.source {
                    continue; // same archive file, nothing to decide
                }
                if loser.file.priority < winner.file.priority {
                    notes.push(format!(
                        "'{}': '{}' (priority {}) overrides '{}' (priority {})",
                        winner.file.dest,
                        winner.file.origin,
                        winner.file.priority,
                        loser.file.origin,
                        loser.file.priority
                    ));
                    continue;
                }
                // Equal priority. Same origin (later mapping in the same
                // option) is defined ordering; identical bytes are moot.
                if loser.file.origin == winner.file.origin
                    || same_content(&installer_root, &loser.file.source, &winner.file.source)?
                {
                    notes.push(format!(
                        "'{}': later mapping from '{}' overrides an earlier one",
                        winner.file.dest, winner.file.origin
                    ));
                    continue;
                }
                ambiguities.push(Ambiguity {
                    dest: winner.file.dest.to_string(),
                    winner: winner.file.origin.clone(),
                    loser: loser.file.origin.clone(),
                });
            }
            files.push(winner.file);
        }
        files.sort_by_key(|f| f.dest.key());
        Ok(InstallPlan {
            files,
            ambiguities,
            notes,
        })
    }
}

fn same_content(root: &Path, a: &RelPath, b: &RelPath) -> Result<bool> {
    let (pa, pb) = (a.to_native(root), b.to_native(root));
    let (ma, mb) = (pa.metadata().path_ctx(&pa)?, pb.metadata().path_ctx(&pb)?);
    if ma.len() != mb.len() {
        return Ok(false);
    }
    Ok(sha256_file(&pa)? == sha256_file(&pb)?)
}

fn file_name_of(p: &RelPath) -> String {
    p.as_str()
        .rsplit('/')
        .next()
        .unwrap_or(p.as_str())
        .to_string()
}

// ---------------------------------------------------------------------------
// Validation (fomod validate): check every mapping and reference without
// running the installer.

#[derive(Debug, Default, Serialize)]
pub struct ValidationReport {
    pub module_name: String,
    pub steps: usize,
    pub groups: usize,
    pub options: usize,
    pub mappings: usize,
    /// Sources referenced by the XML that do not exist in the archive.
    pub missing_sources: Vec<String>,
    /// Destinations (or sources) rejected by path validation.
    pub invalid_paths: Vec<String>,
    /// Condition leaves that cannot be evaluated on this machine.
    pub unsupported_conditions: Vec<String>,
    /// Referenced images that are missing (non-fatal).
    pub missing_images: Vec<String>,
    /// Parser warnings plus anything else non-fatal.
    pub warnings: Vec<String>,
}

impl ValidationReport {
    pub fn fatal(&self) -> bool {
        !self.missing_sources.is_empty() || !self.invalid_paths.is_empty()
    }
}

/// Statically check every mapping, image and condition in the module
/// against the extracted archive. Collects problems instead of failing
/// fast, so one report shows everything.
pub fn validate(
    module: &Module,
    installer_root: &Path,
    env: &dyn Environment,
    rules: DestRules,
) -> Result<ValidationReport> {
    let index = ArchiveIndex::scan(installer_root)?;
    let mut r = ValidationReport {
        module_name: module.name.clone(),
        warnings: module.warnings.clone(),
        ..ValidationReport::default()
    };

    let check_image = |path: &Option<String>, r: &mut ValidationReport, what: &str| {
        if let Some(p) = path {
            match RelPath::parse(p) {
                Ok(rel) if index.files.contains_key(&rel.key()) => {}
                Ok(_) => r.missing_images.push(format!("{what}: {p}")),
                Err(e) => r.invalid_paths.push(format!("{what} image: {e}")),
            }
        }
    };
    check_image(&module.image, &mut r, "module image");

    // Dummy flag set: validation has no selections; conditions referencing
    // flags evaluate against "unset", which is fine for reporting env-only
    // problems (flag conditions are always evaluable).
    let flags = Flags::new();
    let check_cond = |c: &super::model::Composite, r: &mut ValidationReport, what: &str| {
        for line in cond::explain_composite(c, &flags, env) {
            if line.contains("unknown:") {
                r.unsupported_conditions.push(format!("{what}: {line}"));
            }
        }
    };

    let check_mapping = |m: &Mapping, r: &mut ValidationReport, origin: &str| {
        r.mappings += 1;
        if m.is_folder && m.source.trim().is_empty() {
            return; // root expansion is always resolvable
        }
        match RelPath::parse(&m.source) {
            Ok(rel) => {
                let exists = if m.is_folder {
                    index.dirs.contains(&rel.key())
                } else {
                    index.files.contains_key(&rel.key())
                };
                if !exists {
                    r.missing_sources.push(format!("{origin}: {}", m.source));
                }
            }
            Err(e) => r.invalid_paths.push(format!("{origin}: source: {e}")),
        }
        if let Some(d) = m.dest.as_deref()
            && !d.is_empty()
        {
            let stripped = if rules.strip_data_prefix {
                d.split_once('/')
                    .filter(|(first, _)| first.eq_ignore_ascii_case("data"))
                    .map(|(_, rest)| rest)
                    .unwrap_or(d)
            } else {
                d
            };
            if let Err(e) = RelPath::parse(stripped) {
                r.invalid_paths.push(format!("{origin}: destination: {e}"));
            }
        }
    };

    for m in &module.required_files {
        check_mapping(m, &mut r, "required files");
    }
    if let Some(c) = &module.module_dependencies {
        check_cond(c, &mut r, "module dependencies");
    }
    r.steps = module.steps.len();
    for step in &module.steps {
        if let Some(c) = &step.visible {
            check_cond(c, &mut r, &format!("step '{}' visibility", step.name));
        }
        r.groups += step.groups.len();
        for group in &step.groups {
            r.options += group.options.len();
            for opt in &group.options {
                let origin = format!("option '{}'", opt.name);
                check_image(&opt.image, &mut r, &origin);
                for m in &opt.files {
                    check_mapping(m, &mut r, &origin);
                }
                if let TypeDescriptor::Dependent { patterns, .. } = &opt.type_desc {
                    for p in patterns {
                        check_cond(&p.when, &mut r, &format!("{origin} type"));
                    }
                }
            }
        }
    }
    for (i, ci) in module.conditional_installs.iter().enumerate() {
        let origin = format!("conditional install #{}", i + 1);
        check_cond(&ci.when, &mut r, &origin);
        for m in &ci.files {
            check_mapping(m, &mut r, &origin);
        }
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fomod::parse::parse_module_config;
    use crate::fomod::session::Session;

    struct NoEnv;
    impl Environment for NoEnv {
        fn file_state(&self, _: &str) -> Result<super::super::model::FileState> {
            Ok(super::super::model::FileState::Missing)
        }
        fn game_version(&self) -> Result<Option<cond::Version>> {
            Ok(None)
        }
        fn script_extender_version(&self) -> Result<Option<cond::Version>> {
            Ok(None)
        }
    }

    /// Create files (with content = their own path unless given) under a root.
    fn mk(root: &Path, files: &[(&str, &str)]) {
        for (p, content) in files {
            let path = root.join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content.as_bytes()).unwrap();
        }
    }

    /// Run a module with default selections all the way to a plan.
    fn plan_for(
        xml: &str,
        root: &Path,
        rules: DestRules,
        resolutions: &BTreeMap<usize, bool>,
    ) -> Result<BuildOutcome> {
        let module = parse_module_config(xml).unwrap();
        let env = NoEnv;
        let mut session = Session::new(&module, &env);
        let selections = session.finish()?;
        build(&module, &selections, root, &env, rules, resolutions)
    }

    fn dests(plan: &InstallPlan) -> Vec<&str> {
        plan.files.iter().map(|f| f.dest.as_str()).collect()
    }

    #[test]
    fn files_and_folders_map_with_mirroring_and_case_fixup() {
        let t = tempfile::tempdir().unwrap();
        mk(
            t.path(),
            &[
                ("Core/Base.esp", "esp"),
                ("Core/Textures/a.dds", "a"),
                ("Core/Textures/sub/b.dds", "b"),
                ("fomod/ModuleConfig.xml", "unused"),
            ],
        );
        // XML uses different casing than the archive on purpose.
        let xml = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
            <file source="core/base.esp"/>
            <folder source="CORE/TEXTURES" destination="textures"/>
          </requiredInstallFiles></config>"#;
        let out = plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap();
        assert!(out.unresolved.is_empty());
        let plan = out.plan;
        assert!(plan.ambiguities.is_empty());
        // File without destination mirrors its source path (as written).
        assert_eq!(
            dests(&plan),
            vec!["core/base.esp", "textures/a.dds", "textures/sub/b.dds"]
        );
        // Sources carry the archive's true casing.
        assert_eq!(plan.files[0].source.as_str(), "Core/Base.esp");
        assert_eq!(plan.files[2].source.as_str(), "Core/Textures/sub/b.dds");
    }

    #[test]
    fn data_prefix_is_stripped_for_bethesda_layout() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("x/mod.esp", "e")]);
        let xml = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
            <file source="x/mod.esp" destination="Data/mod.esp"/>
          </requiredInstallFiles></config>"#;
        let rules = DestRules {
            strip_data_prefix: true,
        };
        let out = plan_for(xml, t.path(), rules, &BTreeMap::new()).unwrap();
        assert_eq!(dests(&out.plan), vec!["mod.esp"]);
        // Without the rule the prefix stays.
        let out = plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap();
        assert_eq!(dests(&out.plan), vec!["Data/mod.esp"]);
    }

    #[test]
    fn missing_source_and_kind_mismatch_are_errors() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("real.esp", "x"), ("dir/a.txt", "a")]);
        let missing = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
            <file source="ghost.esp"/></requiredInstallFiles></config>"#;
        let err = plan_for(missing, t.path(), DestRules::default(), &BTreeMap::new()).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");

        let dir_as_file = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
            <file source="dir"/></requiredInstallFiles></config>"#;
        let err = plan_for(
            dir_as_file,
            t.path(),
            DestRules::default(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("use <folder>"), "{err}");

        let file_as_dir = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
            <folder source="real.esp"/></requiredInstallFiles></config>"#;
        let err = plan_for(
            file_as_dir,
            t.path(),
            DestRules::default(),
            &BTreeMap::new(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("use <file>"), "{err}");
    }

    #[test]
    fn hostile_paths_are_rejected() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("a.esp", "x")]);
        for dest in ["../escape.esp", "/etc/passwd", "C:\\boot.ini", "a/../../b"] {
            let xml = format!(
                r#"<config><moduleName>m</moduleName><requiredInstallFiles>
                <file source="a.esp" destination="{dest}"/></requiredInstallFiles></config>"#
            );
            let err = plan_for(&xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap_err();
            assert!(
                err.to_string().contains("invalid destination"),
                "{dest}: {err}"
            );
        }
        // Hostile source too.
        let xml = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
            <file source="../../a.esp"/></requiredInstallFiles></config>"#;
        assert!(plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).is_err());
    }

    #[test]
    fn priority_resolves_duplicates_with_note() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("low/f.dds", "low"), ("high/f.dds", "high")]);
        let xml = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
            <file source="low/f.dds" destination="f.dds"/>
            <file source="high/f.dds" destination="f.dds" priority="5"/>
          </requiredInstallFiles></config>"#;
        let out = plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap();
        assert!(out.plan.ambiguities.is_empty());
        assert_eq!(out.plan.files.len(), 1);
        assert_eq!(out.plan.files[0].source.as_str(), "high/f.dds");
        assert!(out.plan.notes.iter().any(|n| n.contains("overrides")));
    }

    #[test]
    fn equal_priority_cross_option_conflict_is_ambiguous() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("a/f.dds", "AAA"), ("b/f.dds", "BBB")]);
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps order="Explicit"><installStep name="S">
            <optionalFileGroups order="Explicit">
              <group name="G" type="SelectAny">
                <plugins order="Explicit">
                  <plugin name="A"><description/><files><file source="a/f.dds" destination="f.dds"/></files>
                    <typeDescriptor><type name="Required"/></typeDescriptor></plugin>
                  <plugin name="B"><description/><files><file source="b/f.dds" destination="f.dds"/></files>
                    <typeDescriptor><type name="Required"/></typeDescriptor></plugin>
                </plugins>
              </group>
            </optionalFileGroups>
          </installStep></installSteps></config>"#;
        let out = plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap();
        assert_eq!(out.plan.ambiguities.len(), 1);
        let amb = &out.plan.ambiguities[0];
        assert!(amb.winner.contains("'B'"), "{amb:?}");
        assert!(amb.loser.contains("'A'"), "{amb:?}");
        // Later option still wins in the file list.
        assert_eq!(out.plan.files[0].source.as_str(), "b/f.dds");
    }

    #[test]
    fn identical_content_is_never_a_conflict() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("a/f.dds", "SAME"), ("b/f.dds", "SAME")]);
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps order="Explicit"><installStep name="S">
            <optionalFileGroups order="Explicit">
              <group name="G" type="SelectAny">
                <plugins order="Explicit">
                  <plugin name="A"><description/><files><file source="a/f.dds" destination="f.dds"/></files>
                    <typeDescriptor><type name="Required"/></typeDescriptor></plugin>
                  <plugin name="B"><description/><files><file source="b/f.dds" destination="f.dds"/></files>
                    <typeDescriptor><type name="Required"/></typeDescriptor></plugin>
                </plugins>
              </group>
            </optionalFileGroups>
          </installStep></installSteps></config>"#;
        let out = plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap();
        assert!(out.plan.ambiguities.is_empty());
        assert_eq!(out.plan.files.len(), 1);
    }

    #[test]
    fn conditional_install_flags_and_unknowns() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("on/x.esp", "x"), ("ver/y.esp", "y")]);
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps order="Explicit"><installStep name="S">
            <optionalFileGroups order="Explicit">
              <group name="G" type="SelectExactlyOne">
                <plugins order="Explicit">
                  <plugin name="On"><description/><files/>
                    <conditionFlags><flag name="x">on</flag></conditionFlags>
                    <typeDescriptor><type name="Recommended"/></typeDescriptor></plugin>
                </plugins>
              </group>
            </optionalFileGroups>
          </installStep></installSteps>
          <conditionalFileInstalls><patterns>
            <pattern>
              <dependencies operator="And"><flagDependency flag="x" value="on"/></dependencies>
              <files><file source="on/x.esp" destination="x.esp"/></files>
            </pattern>
            <pattern>
              <dependencies operator="And"><gameDependency version="1.5"/></dependencies>
              <files><file source="ver/y.esp" destination="y.esp"/></files>
            </pattern>
          </patterns></conditionalFileInstalls></config>"#;
        // First build: the flag pattern applies, the version one is unknown.
        let out = plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap();
        assert_eq!(dests(&out.plan), vec!["x.esp"]);
        assert_eq!(out.unresolved.len(), 1);
        assert!(out.unresolved[0].reason.contains("Linux/Proton"));
        // Resolve as "include".
        let res: BTreeMap<usize, bool> = [(out.unresolved[0].index, true)].into();
        let out = plan_for(xml, t.path(), DestRules::default(), &res).unwrap();
        assert!(out.unresolved.is_empty());
        assert_eq!(dests(&out.plan), vec!["x.esp", "y.esp"]);
        // Resolve as "exclude".
        let res: BTreeMap<usize, bool> = [(1usize, false)].into();
        let out = plan_for(xml, t.path(), DestRules::default(), &res).unwrap();
        assert_eq!(dests(&out.plan), vec!["x.esp"]);
    }

    #[test]
    fn always_install_applies_to_unselected_options() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("opt/readme.txt", "r"), ("opt/big.esp", "b")]);
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps order="Explicit"><installStep name="S">
            <optionalFileGroups order="Explicit">
              <group name="G" type="SelectAny">
                <plugins order="Explicit">
                  <plugin name="NotPicked"><description/>
                    <files>
                      <file source="opt/readme.txt" destination="readme.txt" alwaysInstall="true"/>
                      <file source="opt/big.esp" destination="big.esp"/>
                    </files>
                    <typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
                </plugins>
              </group>
            </optionalFileGroups>
          </installStep></installSteps></config>"#;
        let out = plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap();
        // Only the alwaysInstall file lands; the option itself is unselected.
        assert_eq!(dests(&out.plan), vec!["readme.txt"]);
        assert!(out.plan.files[0].origin.contains("alwaysInstall"));
    }

    #[test]
    fn empty_folder_source_expands_root_minus_fomod() {
        let t = tempfile::tempdir().unwrap();
        mk(
            t.path(),
            &[
                ("mod.esp", "e"),
                ("textures/a.dds", "a"),
                ("fomod/ModuleConfig.xml", "cfg"),
            ],
        );
        let xml = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
            <folder source="" destination=""/></requiredInstallFiles></config>"#;
        let out = plan_for(xml, t.path(), DestRules::default(), &BTreeMap::new()).unwrap();
        assert_eq!(dests(&out.plan), vec!["mod.esp", "textures/a.dds"]);
    }

    #[test]
    fn validate_reports_everything_without_failing() {
        let t = tempfile::tempdir().unwrap();
        mk(t.path(), &[("ok.esp", "x")]);
        let xml = r#"<config><moduleName>m</moduleName>
          <moduleImage path="fomod/missing.png"/>
          <requiredInstallFiles>
            <file source="ok.esp"/>
            <file source="ghost.esp"/>
            <file source="ok.esp" destination="../bad"/>
          </requiredInstallFiles>
          <installSteps order="Explicit"><installStep name="S">
            <visible><gameDependency version="1.5"/></visible>
            <optionalFileGroups order="Explicit">
              <group name="G" type="SelectAny"><plugins order="Explicit">
                <plugin name="P"><description/><files/><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
              </plugins></group>
            </optionalFileGroups>
          </installStep></installSteps></config>"#;
        let module = parse_module_config(xml).unwrap();
        let env = NoEnv;
        let r = validate(&module, t.path(), &env, DestRules::default()).unwrap();
        assert_eq!(r.steps, 1);
        assert_eq!(r.groups, 1);
        assert_eq!(r.options, 1);
        assert_eq!(r.mappings, 3);
        assert_eq!(r.missing_sources.len(), 1);
        assert_eq!(r.invalid_paths.len(), 1);
        assert_eq!(r.missing_images.len(), 1);
        assert_eq!(r.unsupported_conditions.len(), 1);
        assert!(r.fatal());
    }
}
