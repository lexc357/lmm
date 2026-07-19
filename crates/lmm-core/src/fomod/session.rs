//! The interactive selection session: a pure state machine over a parsed
//! [`Module`].
//!
//! The frontend (terminal UI in `lmm-cli`) renders [`StepView`]s and calls
//! the mutators; nothing here prints or reads input, and nothing touches
//! the filesystem — the session ends in [`Selections`], which the plan
//! builder turns into files.
//!
//! Semantics implemented (matching mainstream managers where the spec is
//! vague):
//! * Step visibility and flags are recomputed from step 0 forward on every
//!   query: a step's flags only count while the step itself is visible and
//!   only affect *later* steps. Changing an early choice therefore
//!   re-evaluates everything after it; selections made in a step that
//!   became invisible are kept but ignored (they come back if the step
//!   does).
//! * Option types are evaluated against the flags in force *before* the
//!   option's step, so same-step flag dependencies cannot oscillate.
//! * A step whose visibility cannot be determined (Unknown) is shown, with
//!   the reason attached — hiding it could silently drop files.
//! * Auto-selection on first entry: Required and Recommended options, all
//!   options of SelectAll groups, and a first selectable option for
//!   SelectExactlyOne/SelectAtLeastOne groups that would otherwise start
//!   empty (radio groups always have a current value, like the reference
//!   UIs).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

use super::cond::{self, Environment, Eval, Flags};
use super::model::{GroupRule, Module, OptionType, TypeDescriptor};

/// A running installer session.
pub struct Session<'a> {
    module: &'a Module,
    env: &'a dyn Environment,
    /// selections[step][group] = selected option indices (module indexing).
    selections: Vec<Vec<BTreeSet<usize>>>,
    /// Whether auto-selection ran for a step yet.
    initialized: Vec<bool>,
    /// Position within the *currently visible* steps.
    cursor: usize,
}

/// Snapshot of one step for rendering.
#[derive(Debug)]
pub struct StepView {
    /// Index into `module.steps`.
    pub index: usize,
    pub name: String,
    /// Present when visibility evaluated to Unknown: the reason the step is
    /// shown anyway.
    pub visibility_note: Option<String>,
    pub groups: Vec<GroupView>,
}

#[derive(Debug)]
pub struct GroupView {
    pub index: usize,
    pub name: String,
    pub rule: GroupRule,
    pub options: Vec<OptionView>,
}

#[derive(Debug)]
pub struct OptionView {
    pub index: usize,
    pub name: String,
    pub description: String,
    pub image: Option<String>,
    /// Resolved type under the current flags/environment.
    pub kind: OptionType,
    pub selected: bool,
    /// Why the option cannot be toggled (required / not usable / group
    /// rule), if it can't.
    pub locked: Option<String>,
    /// Non-blocking caveat (CouldBeUsable explanation, unknown-condition
    /// note on a dependent type).
    pub note: Option<String>,
}

/// The completed choices, in stable, serializable form. Option identity is
/// (name, index) at every level: FOMOD has no ids, so names are the stable
/// identifiers and indices disambiguate duplicates.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Selections {
    pub steps: Vec<StepChoice>,
    /// Final flag values after the last visible step.
    pub flags: Flags,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepChoice {
    pub step_index: usize,
    pub step: String,
    pub groups: Vec<GroupChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GroupChoice {
    pub group_index: usize,
    pub group: String,
    pub options: Vec<OptionChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OptionChoice {
    pub option_index: usize,
    pub option: String,
}

impl<'a> Session<'a> {
    pub fn new(module: &'a Module, env: &'a dyn Environment) -> Session<'a> {
        let selections = module
            .steps
            .iter()
            .map(|s| vec![BTreeSet::new(); s.groups.len()])
            .collect();
        let mut session = Session {
            module,
            env,
            selections,
            initialized: vec![false; module.steps.len()],
            cursor: 0,
        };
        if let Some(&first) = session.visible_steps().first() {
            session.initialize_step(first);
        }
        session
    }

    /// Re-open a session with saved choices preselected where they still
    /// resolve. Returns notes about anything that no longer applies.
    pub fn with_preselected(
        module: &'a Module,
        env: &'a dyn Environment,
        saved: &Selections,
    ) -> (Session<'a>, Vec<String>) {
        let mut session = Session::new(module, env);
        let mut notes = Vec::new();
        for sc in &saved.steps {
            // Match by name first (survives reordering), index as tiebreak.
            let Some(si) = find_named(
                module.steps.iter().map(|s| s.name.as_str()),
                &sc.step,
                sc.step_index,
            ) else {
                notes.push(format!("saved step '{}' no longer exists", sc.step));
                continue;
            };
            for gc in &sc.groups {
                let step = &module.steps[si];
                let Some(gi) = find_named(
                    step.groups.iter().map(|g| g.name.as_str()),
                    &gc.group,
                    gc.group_index,
                ) else {
                    notes.push(format!(
                        "saved group '{}' no longer exists in step '{}'",
                        gc.group, sc.step
                    ));
                    continue;
                };
                for oc in &gc.options {
                    let group = &step.groups[gi];
                    let Some(oi) = find_named(
                        group.options.iter().map(|o| o.name.as_str()),
                        &oc.option,
                        oc.option_index,
                    ) else {
                        notes.push(format!(
                            "saved option '{}' no longer exists in group '{}'",
                            oc.option, gc.group
                        ));
                        continue;
                    };
                    // Overwrite the defaults with the saved state; radio
                    // rules are enforced by inserting in saved order.
                    match group.rule {
                        GroupRule::ExactlyOne | GroupRule::AtMostOne => {
                            session.selections[si][gi].clear();
                        }
                        _ => {}
                    }
                    session.selections[si][gi].insert(oi);
                    session.initialized[si] = true;
                }
            }
        }
        // Preselected NotUsable options must not survive: drop them the
        // same way a fresh toggle would refuse them.
        session.drop_unusable(&mut notes);
        (session, notes)
    }

    pub fn module(&self) -> &Module {
        self.module
    }

    /// Indices of the steps currently visible, walking flags forward.
    pub fn visible_steps(&self) -> Vec<usize> {
        let mut visible = Vec::new();
        let mut flags = Flags::new();
        for (i, step) in self.module.steps.iter().enumerate() {
            let show = match &step.visible {
                None => true,
                Some(c) => !matches!(cond::eval_composite(c, &flags, self.env), Eval::False),
            };
            if show {
                visible.push(i);
                self.apply_flags(i, &mut flags);
            }
        }
        visible
    }

    /// (1-based position, total) within the visible steps.
    pub fn position(&self) -> (usize, usize) {
        let total = self.visible_steps().len();
        (self.cursor.min(total.saturating_sub(1)) + 1, total)
    }

    /// Flags in force *before* the visible step at `step_index`.
    fn flags_before(&self, step_index: usize) -> Flags {
        let mut flags = Flags::new();
        for &i in &self.visible_steps() {
            if i >= step_index {
                break;
            }
            self.apply_flags(i, &mut flags);
        }
        flags
    }

    /// Final flags after every visible step.
    pub fn final_flags(&self) -> Flags {
        let mut flags = Flags::new();
        for &i in &self.visible_steps() {
            self.apply_flags(i, &mut flags);
        }
        flags
    }

    fn apply_flags(&self, step_index: usize, flags: &mut Flags) {
        let step = &self.module.steps[step_index];
        for (gi, group) in step.groups.iter().enumerate() {
            for &oi in &self.selections[step_index][gi] {
                for f in &group.options[oi].flags {
                    flags.insert(f.name.clone(), f.value.clone());
                }
            }
        }
    }

    /// Resolve an option's effective type under the flags before its step.
    /// Returns the type and, for pattern-derived results, why.
    fn resolve_type(&self, td: &TypeDescriptor, flags: &Flags) -> (OptionType, Option<String>) {
        match td {
            TypeDescriptor::Simple(t) => (*t, None),
            TypeDescriptor::Dependent { default, patterns } => {
                for p in patterns {
                    match cond::eval_composite(&p.when, flags, self.env) {
                        Eval::True => {
                            return (p.becomes, Some(cond::describe_composite(&p.when)));
                        }
                        Eval::False => {}
                        Eval::Unknown(why) => {
                            // Guessing a pattern type could lock or unlock
                            // the wrong option; fall back to the default
                            // and say why.
                            return (
                                *default,
                                Some(format!("condition could not be checked ({why})")),
                            );
                        }
                    }
                }
                // No pattern matched: explain what would have changed the
                // outcome ("requires flag 'mode' = 'full'").
                let unmet: Vec<String> = patterns
                    .iter()
                    .filter(|p| p.becomes != *default)
                    .map(|p| cond::describe_composite(&p.when))
                    .collect();
                let why = (!unmet.is_empty()).then(|| format!("requires {}", unmet.join(" or ")));
                (*default, why)
            }
        }
    }

    /// The current step, rendered for the frontend.
    pub fn current(&mut self) -> Result<StepView> {
        let visible = self.visible_steps();
        if visible.is_empty() {
            return Err(Error::Fomod("installer has no visible steps".into()));
        }
        self.cursor = self.cursor.min(visible.len() - 1);
        let si = visible[self.cursor];
        self.initialize_step(si);
        Ok(self.view_step(si))
    }

    fn view_step(&self, si: usize) -> StepView {
        let step = &self.module.steps[si];
        let flags = self.flags_before(si);
        let visibility_note = step.visible.as_ref().and_then(|c| {
            match cond::eval_composite(c, &self.flags_before(si), self.env) {
                Eval::Unknown(why) => Some(format!(
                    "shown although its condition could not be checked: {why}"
                )),
                _ => None,
            }
        });
        let groups = step
            .groups
            .iter()
            .enumerate()
            .map(|(gi, group)| GroupView {
                index: gi,
                name: group.name.clone(),
                rule: group.rule,
                options: group
                    .options
                    .iter()
                    .enumerate()
                    .map(|(oi, opt)| {
                        let (kind, why) = self.resolve_type(&opt.type_desc, &flags);
                        let selected = self.selections[si][gi].contains(&oi);
                        let locked = match kind {
                            OptionType::Required => Some("required by the installer".to_string()),
                            OptionType::NotUsable => Some(match &why {
                                Some(w) => format!("not usable here: {w}"),
                                None => "marked not usable by the installer".to_string(),
                            }),
                            _ if group.rule == GroupRule::All => {
                                Some("this group installs all of its options".to_string())
                            }
                            _ => None,
                        };
                        let note = match kind {
                            OptionType::CouldBeUsable => Some(match &why {
                                Some(w) => format!("may not work as-is: {w}"),
                                None => "the installer marks this 'could be usable'".to_string(),
                            }),
                            OptionType::Recommended => Some(match &why {
                                Some(w) => format!("recommended because {w}"),
                                None => "recommended".to_string(),
                            }),
                            _ => why.map(|w| format!("type set by: {w}")),
                        };
                        OptionView {
                            index: oi,
                            name: opt.name.clone(),
                            description: opt.description.clone(),
                            image: opt.image.clone(),
                            kind,
                            selected,
                            locked,
                            note,
                        }
                    })
                    .collect(),
            })
            .collect();
        StepView {
            index: si,
            name: step.name.clone(),
            visibility_note,
            groups,
        }
    }

    /// First-entry auto-selection (idempotent).
    fn initialize_step(&mut self, si: usize) {
        if self.initialized[si] {
            return;
        }
        self.initialized[si] = true;
        let flags = self.flags_before(si);
        let step = &self.module.steps[si];
        for (gi, group) in step.groups.iter().enumerate() {
            let mut kinds = Vec::with_capacity(group.options.len());
            for opt in &group.options {
                kinds.push(self.resolve_type(&opt.type_desc, &flags).0);
            }
            let sel = &mut self.selections[si][gi];
            for (oi, kind) in kinds.iter().enumerate() {
                let auto = match kind {
                    OptionType::Required | OptionType::Recommended => true,
                    OptionType::NotUsable => false,
                    _ => group.rule == GroupRule::All,
                };
                if auto {
                    sel.insert(oi);
                }
            }
            // Radio-style groups start with a value; pick the first usable
            // option if nothing was auto-selected.
            if sel.is_empty()
                && matches!(group.rule, GroupRule::ExactlyOne | GroupRule::AtLeastOne)
                && let Some(first) = kinds.iter().position(|k| *k != OptionType::NotUsable)
            {
                sel.insert(first);
            }
            // ExactlyOne/AtMostOne with several auto-selections (e.g. two
            // Recommended): keep the first only.
            if matches!(group.rule, GroupRule::ExactlyOne | GroupRule::AtMostOne) && sel.len() > 1 {
                let keep = *sel.iter().next().expect("non-empty");
                sel.retain(|&oi| oi == keep);
            }
        }
    }

    /// Deselect NotUsable options everywhere (used after preselection).
    fn drop_unusable(&mut self, notes: &mut Vec<String>) {
        for si in 0..self.module.steps.len() {
            let flags = self.flags_before(si);
            let step = &self.module.steps[si];
            for (gi, group) in step.groups.iter().enumerate() {
                let drop: Vec<usize> = self.selections[si][gi]
                    .iter()
                    .copied()
                    .filter(|&oi| {
                        self.resolve_type(&group.options[oi].type_desc, &flags).0
                            == OptionType::NotUsable
                    })
                    .collect();
                for oi in drop {
                    self.selections[si][gi].remove(&oi);
                    notes.push(format!(
                        "'{}' in group '{}' is no longer usable and was deselected",
                        group.options[oi].name, group.name
                    ));
                }
            }
        }
    }

    /// Select an option in the current step. Radio rules replace the
    /// previous selection; locked options refuse with the reason.
    pub fn select(&mut self, group_index: usize, option_index: usize) -> Result<()> {
        let (si, view) = self.current_indices(group_index, option_index)?;
        let opt = &view.groups[group_index].options[option_index];
        if opt.kind == OptionType::NotUsable {
            return Err(Error::Fomod(format!(
                "'{}' cannot be selected — {}",
                opt.name,
                opt.locked.as_deref().unwrap_or("not usable")
            )));
        }
        let rule = self.module.steps[si].groups[group_index].rule;
        if matches!(rule, GroupRule::ExactlyOne | GroupRule::AtMostOne) {
            // A Required option in a radio group is the only valid pick.
            if let Some(req) = view.groups[group_index]
                .options
                .iter()
                .find(|o| o.kind == OptionType::Required && o.index != option_index)
            {
                return Err(Error::Fomod(format!(
                    "'{}' is required in group '{}' and cannot be replaced",
                    req.name, view.groups[group_index].name
                )));
            }
            self.selections[si][group_index].clear();
        }
        self.selections[si][group_index].insert(option_index);
        Ok(())
    }

    pub fn deselect(&mut self, group_index: usize, option_index: usize) -> Result<()> {
        let (si, view) = self.current_indices(group_index, option_index)?;
        let opt = &view.groups[group_index].options[option_index];
        let rule = self.module.steps[si].groups[group_index].rule;
        // Required options and SelectAll groups are pinned; a NotUsable
        // option that somehow ended up selected may always be removed.
        if opt.selected && opt.kind == OptionType::Required {
            return Err(Error::Fomod(format!(
                "'{}' is required by the installer and cannot be deselected",
                opt.name
            )));
        }
        if opt.selected && rule == GroupRule::All && opt.kind != OptionType::NotUsable {
            return Err(Error::Fomod(format!(
                "group '{}' installs all of its options; '{}' cannot be deselected",
                view.groups[group_index].name, opt.name
            )));
        }
        self.selections[si][group_index].remove(&option_index);
        Ok(())
    }

    /// Select every usable option of a group (for `all`).
    pub fn select_all(&mut self, group_index: usize) -> Result<()> {
        let view = self.current()?;
        let group = view
            .groups
            .get(group_index)
            .ok_or_else(|| Error::Fomod(format!("no group {}", group_index + 1)))?;
        if matches!(group.rule, GroupRule::ExactlyOne | GroupRule::AtMostOne)
            && group.options.len() > 1
        {
            return Err(Error::Fomod(format!(
                "group '{}' allows only one selection",
                group.name
            )));
        }
        for opt in &group.options {
            if opt.kind != OptionType::NotUsable {
                self.select(group_index, opt.index)?;
            }
        }
        Ok(())
    }

    /// Deselect everything deselectable in a group (for `none`).
    pub fn select_none(&mut self, group_index: usize) -> Result<()> {
        let view = self.current()?;
        let group = view
            .groups
            .get(group_index)
            .ok_or_else(|| Error::Fomod(format!("no group {}", group_index + 1)))?;
        for opt in &group.options {
            if opt.selected && opt.locked.is_none() {
                self.deselect(group_index, opt.index)?;
            }
        }
        Ok(())
    }

    fn current_indices(
        &mut self,
        group_index: usize,
        option_index: usize,
    ) -> Result<(usize, StepView)> {
        let view = self.current()?;
        let group = view
            .groups
            .get(group_index)
            .ok_or_else(|| Error::Fomod(format!("no group {}", group_index + 1)))?;
        if option_index >= group.options.len() {
            return Err(Error::Fomod(format!(
                "group '{}' has no option {}",
                group.name,
                option_index + 1
            )));
        }
        Ok((view.index, view))
    }

    /// Group-rule violations in the current step (empty = may advance).
    pub fn validate_current(&mut self) -> Result<Vec<String>> {
        let view = self.current()?;
        Ok(validate_view(&view))
    }

    /// Advance. Returns false when the current step was the last visible
    /// one (the session is complete). Refuses to advance past violations.
    pub fn advance(&mut self) -> Result<bool> {
        let violations = self.validate_current()?;
        if !violations.is_empty() {
            return Err(Error::Fomod(violations.join("; ")));
        }
        let total = self.visible_steps().len();
        if self.cursor + 1 >= total {
            return Ok(false);
        }
        self.cursor += 1;
        // Ensure the newly-entered step is initialized for rendering.
        self.current()?;
        Ok(true)
    }

    /// Go back one visible step. Returns false when already at the first.
    pub fn back(&mut self) -> bool {
        if self.cursor == 0 {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Validate every visible step and produce the final [`Selections`].
    /// Steps never entered get their defaults applied first.
    pub fn finish(&mut self) -> Result<Selections> {
        let visible = self.visible_steps();
        for &si in &visible {
            self.initialize_step(si);
        }
        // Visibility may shift as defaults set flags; fetch again.
        let visible = self.visible_steps();
        let mut steps = Vec::new();
        for &si in &visible {
            self.initialize_step(si);
            let view = self.view_step(si);
            let violations = validate_view(&view);
            if !violations.is_empty() {
                return Err(Error::Fomod(format!(
                    "step '{}': {}",
                    view.name,
                    violations.join("; ")
                )));
            }
            let step = &self.module.steps[si];
            let groups: Vec<GroupChoice> = step
                .groups
                .iter()
                .enumerate()
                .filter(|(gi, _)| !self.selections[si][*gi].is_empty())
                .map(|(gi, group)| GroupChoice {
                    group_index: gi,
                    group: group.name.clone(),
                    options: self.selections[si][gi]
                        .iter()
                        .map(|&oi| OptionChoice {
                            option_index: oi,
                            option: group.options[oi].name.clone(),
                        })
                        .collect(),
                })
                .collect();
            steps.push(StepChoice {
                step_index: si,
                step: step.name.clone(),
                groups,
            });
        }
        Ok(Selections {
            steps,
            flags: self.final_flags(),
        })
    }

    /// The module-level indices of selected options, for the plan builder:
    /// (step, group, option) in module order.
    pub fn selected_indices(&self) -> Vec<(usize, usize, usize)> {
        let mut out = Vec::new();
        for &si in &self.visible_steps() {
            for (gi, sel) in self.selections[si].iter().enumerate() {
                for &oi in sel {
                    out.push((si, gi, oi));
                }
            }
        }
        out
    }
}

/// Group-rule violations for a rendered step.
fn validate_view(view: &StepView) -> Vec<String> {
    let mut violations = Vec::new();
    for group in &view.groups {
        let n = group.options.iter().filter(|o| o.selected).count();
        let bad = match group.rule {
            GroupRule::ExactlyOne => n != 1,
            GroupRule::AtMostOne => n > 1,
            GroupRule::AtLeastOne => n == 0,
            GroupRule::Any => false,
            // All: every usable option must be selected.
            GroupRule::All => group
                .options
                .iter()
                .any(|o| o.kind != OptionType::NotUsable && !o.selected),
        };
        if bad {
            violations.push(format!(
                "group '{}' needs: {} ({} selected)",
                group.name,
                group.rule.describe(),
                n
            ));
        }
    }
    violations
}

/// Find an item by exact name; on duplicates, prefer the saved index.
fn find_named<'x>(
    names: impl Iterator<Item = &'x str>,
    wanted: &str,
    saved_index: usize,
) -> Option<usize> {
    let matches: Vec<usize> = names
        .enumerate()
        .filter(|(_, n)| *n == wanted)
        .map(|(i, _)| i)
        .collect();
    match matches.as_slice() {
        [] => None,
        [one] => Some(*one),
        several => Some(
            several
                .iter()
                .copied()
                .find(|&i| i == saved_index)
                .unwrap_or(several[0]),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fomod::parse::parse_module_config;

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

    fn module(xml: &str) -> Module {
        parse_module_config(xml).unwrap()
    }

    const TWO_STEP: &str = r#"<config><moduleName>m</moduleName>
      <installSteps order="Explicit">
        <installStep name="Main">
          <optionalFileGroups order="Explicit">
            <group name="Core" type="SelectExactlyOne">
              <plugins order="Explicit">
                <plugin name="A"><description/><files/><conditionFlags><flag name="pick">a</flag></conditionFlags>
                  <typeDescriptor><type name="Recommended"/></typeDescriptor></plugin>
                <plugin name="B"><description/><files/><conditionFlags><flag name="pick">b</flag></conditionFlags>
                  <typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
              </plugins>
            </group>
            <group name="Extras" type="SelectAny">
              <plugins order="Explicit">
                <plugin name="X"><description/><files/><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
                <plugin name="Y"><description/><files/><typeDescriptor><type name="Required"/></typeDescriptor></plugin>
                <plugin name="Z"><description/><files/><typeDescriptor><type name="NotUsable"/></typeDescriptor></plugin>
              </plugins>
            </group>
          </optionalFileGroups>
        </installStep>
        <installStep name="OnlyForA">
          <visible><flagDependency flag="pick" value="a"/></visible>
          <optionalFileGroups order="Explicit">
            <group name="G2" type="SelectAtLeastOne">
              <plugins order="Explicit">
                <plugin name="P"><description/><files/><typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
              </plugins>
            </group>
          </optionalFileGroups>
        </installStep>
      </installSteps></config>"#;

    #[test]
    fn auto_selection_on_first_step() {
        let m = module(TWO_STEP);
        let env = NoEnv;
        let mut s = Session::new(&m, &env);
        let view = s.current().unwrap();
        // Recommended "A" preselected in the radio group.
        assert!(view.groups[0].options[0].selected);
        assert!(!view.groups[0].options[1].selected);
        // Required "Y" selected and locked; NotUsable "Z" locked out.
        let extras = &view.groups[1];
        assert!(extras.options[1].selected);
        assert!(extras.options[1].locked.is_some());
        assert!(!extras.options[2].selected);
        assert!(extras.options[2].locked.is_some());
    }

    #[test]
    fn radio_select_replaces_previous() {
        let m = module(TWO_STEP);
        let env = NoEnv;
        let mut s = Session::new(&m, &env);
        s.select(0, 1).unwrap(); // pick B
        let view = s.current().unwrap();
        assert!(!view.groups[0].options[0].selected);
        assert!(view.groups[0].options[1].selected);
    }

    #[test]
    fn required_cannot_be_deselected_notusable_cannot_be_selected() {
        let m = module(TWO_STEP);
        let env = NoEnv;
        let mut s = Session::new(&m, &env);
        let err = s.deselect(1, 1).unwrap_err();
        assert!(err.to_string().contains("required"), "{err}");
        let err = s.select(1, 2).unwrap_err();
        assert!(err.to_string().contains("cannot be selected"), "{err}");
    }

    #[test]
    fn flag_controls_step_visibility() {
        let m = module(TWO_STEP);
        let env = NoEnv;
        let mut s = Session::new(&m, &env);
        // Default pick A -> flag pick=a -> step 2 visible.
        assert_eq!(s.visible_steps(), vec![0, 1]);
        assert_eq!(s.position(), (1, 2));
        s.select(0, 1).unwrap(); // pick B
        assert_eq!(s.visible_steps(), vec![0]);
        assert_eq!(s.position(), (1, 1));
        // Selections survive a visibility roundtrip.
        s.select(0, 0).unwrap();
        assert_eq!(s.visible_steps(), vec![0, 1]);
    }

    #[test]
    fn next_back_and_finish() {
        let m = module(TWO_STEP);
        let env = NoEnv;
        let mut s = Session::new(&m, &env);
        assert!(s.advance().unwrap()); // -> step 2 (auto-inits: AtLeastOne picks P)
        let view = s.current().unwrap();
        assert_eq!(view.name, "OnlyForA");
        assert!(view.groups[0].options[0].selected, "AtLeastOne default");
        assert!(!s.advance().unwrap(), "last step: session complete");
        assert!(s.back());
        assert!(!s.back(), "already at first");

        let sel = s.finish().unwrap();
        assert_eq!(sel.steps.len(), 2);
        assert_eq!(sel.flags.get("pick").map(String::as_str), Some("a"));
        let first = &sel.steps[0];
        assert_eq!(first.groups[0].options[0].option, "A");
    }

    #[test]
    fn validation_blocks_next() {
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps order="Explicit"><installStep name="S">
            <optionalFileGroups order="Explicit">
              <group name="Pick" type="SelectAtLeastOne">
                <plugins order="Explicit">
                  <plugin name="OnlyNotUsable"><description/><files/>
                    <typeDescriptor><type name="NotUsable"/></typeDescriptor></plugin>
                </plugins>
              </group>
            </optionalFileGroups>
          </installStep></installSteps></config>"#;
        let m = module(xml);
        let env = NoEnv;
        let mut s = Session::new(&m, &env);
        // Nothing selectable: AtLeastOne cannot be satisfied.
        let err = s.advance().unwrap_err();
        assert!(err.to_string().contains("at least one"), "{err}");
        assert!(s.finish().is_err());
    }

    #[test]
    fn dependency_type_locks_by_flag() {
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps order="Explicit">
            <installStep name="One">
              <optionalFileGroups order="Explicit">
                <group name="Mode" type="SelectExactlyOne">
                  <plugins order="Explicit">
                    <plugin name="Lite"><description/><files/><conditionFlags><flag name="mode">lite</flag></conditionFlags>
                      <typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
                    <plugin name="Full"><description/><files/><conditionFlags><flag name="mode">full</flag></conditionFlags>
                      <typeDescriptor><type name="Optional"/></typeDescriptor></plugin>
                  </plugins>
                </group>
              </optionalFileGroups>
            </installStep>
            <installStep name="Two">
              <optionalFileGroups order="Explicit">
                <group name="Patch" type="SelectAny">
                  <plugins order="Explicit">
                    <plugin name="FullOnly"><description/><files/>
                      <typeDescriptor><dependencyType><defaultType name="NotUsable"/>
                        <patterns><pattern>
                          <dependencies operator="And"><flagDependency flag="mode" value="full"/></dependencies>
                          <type name="Optional"/>
                        </pattern></patterns>
                      </dependencyType></typeDescriptor></plugin>
                  </plugins>
                </group>
              </optionalFileGroups>
            </installStep>
          </installSteps></config>"#;
        let m = module(xml);
        let env = NoEnv;
        let mut s = Session::new(&m, &env);
        // Default pick = Lite (first selectable) -> FullOnly NotUsable.
        s.advance().unwrap();
        let view = s.current().unwrap();
        let opt = &view.groups[0].options[0];
        assert_eq!(opt.kind, OptionType::NotUsable);
        assert!(opt.locked.as_deref().unwrap().contains("mode"), "{opt:?}");
        // Change step 1 to Full -> FullOnly becomes selectable.
        assert!(s.back());
        s.select(0, 1).unwrap();
        s.advance().unwrap();
        let view = s.current().unwrap();
        assert_eq!(view.groups[0].options[0].kind, OptionType::Optional);
        s.select(0, 0).unwrap();
    }

    #[test]
    fn preselection_restores_and_drops_invalid() {
        let m = module(TWO_STEP);
        let env = NoEnv;
        let saved = Selections {
            steps: vec![StepChoice {
                step_index: 0,
                step: "Main".into(),
                groups: vec![
                    GroupChoice {
                        group_index: 0,
                        group: "Core".into(),
                        options: vec![OptionChoice {
                            option_index: 1,
                            option: "B".into(),
                        }],
                    },
                    GroupChoice {
                        group_index: 1,
                        group: "Extras".into(),
                        options: vec![OptionChoice {
                            option_index: 9,
                            option: "Gone".into(),
                        }],
                    },
                ],
            }],
            flags: Flags::new(),
        };
        let (mut s, notes) = Session::with_preselected(&m, &env, &saved);
        assert!(notes.iter().any(|n| n.contains("Gone")), "{notes:?}");
        let view = s.current().unwrap();
        assert!(view.groups[0].options[1].selected, "saved pick B restored");
        assert!(!view.groups[0].options[0].selected);
    }

    #[test]
    fn no_steps_module_finishes_empty() {
        let m = module("<config><moduleName>m</moduleName></config>");
        let env = NoEnv;
        let mut s = Session::new(&m, &env);
        let sel = s.finish().unwrap();
        assert!(sel.steps.is_empty());
        assert!(s.current().is_err());
    }
}
