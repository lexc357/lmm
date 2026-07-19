//! The FOMOD data model: plain, order-preserving data produced by
//! [`crate::fomod::parse`] and consumed by the session, planner and UI.
//!
//! Nothing here touches the filesystem or the environment. All strings are
//! untrusted archive content: paths (`Mapping::source`/`dest`, image paths)
//! are stored raw and validated with [`crate::paths::RelPath`] at the point
//! of use, never joined onto a directory before that.

use serde::Serialize;

/// Metadata from `fomod/info.xml`. Purely informational; every field is
/// optional because real-world info.xml files omit most of them.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ModuleInfo {
    pub name: Option<String>,
    pub author: Option<String>,
    pub version: Option<String>,
    pub description: Option<String>,
    pub website: Option<String>,
}

/// A parsed `ModuleConfig.xml`.
#[derive(Debug, Clone, Serialize)]
pub struct Module {
    /// `<moduleName>`; falls back to the info.xml name or "(unnamed module)".
    pub name: String,
    /// `<moduleImage path=...>`, raw archive-relative path.
    pub image: Option<String>,
    /// `<moduleDependencies>`: preconditions for installing the mod at all.
    pub module_dependencies: Option<Composite>,
    /// `<requiredInstallFiles>`: installed regardless of any choice.
    pub required_files: Vec<Mapping>,
    /// `<installSteps>`, already in presentation order (see `order` note in
    /// the parser: Ascending is the schema default, Explicit keeps document
    /// order).
    pub steps: Vec<Step>,
    /// `<conditionalFileInstalls>` patterns, evaluated against the final
    /// flag set after the last step.
    pub conditional_installs: Vec<ConditionalInstall>,
    /// Tolerated-but-noted parsing irregularities (unknown elements, clamped
    /// text, unsorted duplicates). Shown by `fomod validate` and verbose
    /// installs; never fatal by themselves.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub name: String,
    /// `<visible>`: condition for showing this step. `None` = always.
    pub visible: Option<Composite>,
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Group {
    pub name: String,
    pub rule: GroupRule,
    pub options: Vec<OptionDef>,
}

/// `<group type=...>`: how many options may/must be selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum GroupRule {
    ExactlyOne,
    AtMostOne,
    AtLeastOne,
    Any,
    All,
}

impl GroupRule {
    /// Human phrasing used by the UI and validation messages.
    pub fn describe(self) -> &'static str {
        match self {
            GroupRule::ExactlyOne => "select exactly one",
            GroupRule::AtMostOne => "select at most one",
            GroupRule::AtLeastOne => "select at least one",
            GroupRule::Any => "select any number",
            GroupRule::All => "all options are required",
        }
    }
}

/// `<plugin>`: one selectable option.
#[derive(Debug, Clone, Serialize)]
pub struct OptionDef {
    pub name: String,
    pub description: String,
    /// Raw archive-relative image path, if any.
    pub image: Option<String>,
    /// Files installed when this option is selected.
    pub files: Vec<Mapping>,
    /// Flags set when this option is selected (`<conditionFlags>`).
    pub flags: Vec<FlagSet>,
    pub type_desc: TypeDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FlagSet {
    pub name: String,
    pub value: String,
}

/// `<typeDescriptor>`: either a fixed type or one derived from conditions.
#[derive(Debug, Clone, Serialize)]
pub enum TypeDescriptor {
    Simple(OptionType),
    /// `<dependencyType>`: first matching pattern wins, else the default.
    Dependent {
        default: OptionType,
        patterns: Vec<TypePattern>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct TypePattern {
    pub when: Composite,
    pub becomes: OptionType,
}

/// FOMOD plugin types, mapped to terminal behavior by the session:
/// Required = selected and locked; Recommended = preselected;
/// NotUsable = locked out; CouldBeUsable = selectable with a warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum OptionType {
    Required,
    Recommended,
    Optional,
    NotUsable,
    CouldBeUsable,
}

/// One `<file>` or `<folder>` mapping. Paths are raw strings from the XML;
/// the planner validates and resolves them.
#[derive(Debug, Clone, Serialize)]
pub struct Mapping {
    /// Archive path relative to the installer root (the dir holding fomod/).
    pub source: String,
    /// Destination relative to the game's mod root. `None` mirrors the
    /// source path for files, or maps folder contents onto the root.
    pub dest: Option<String>,
    pub is_folder: bool,
    /// FOMOD priority: on duplicate destinations, higher priority wins.
    pub priority: i64,
    /// `alwaysInstall`: install even when the option is not selected.
    pub always_install: bool,
    /// `installIfUsable`: install whenever the option's type is not
    /// NotUsable, selected or not.
    pub install_if_usable: bool,
}

/// `<conditionalFileInstalls>` pattern.
#[derive(Debug, Clone, Serialize)]
pub struct ConditionalInstall {
    pub when: Composite,
    pub files: Vec<Mapping>,
}

/// A dependency tree: `<dependencies operator="And|Or">` with leaves.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Composite {
    pub op: Op,
    pub parts: Vec<Condition>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Op {
    And,
    Or,
}

/// A single dependency. FOMOD has no NOT operator; `File{state: Missing}`
/// is the format's negation.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum Condition {
    /// `<fileDependency file=... state=...>`.
    File { file: String, state: FileState },
    /// `<flagDependency flag=... value=...>`.
    Flag { flag: String, value: String },
    /// `<gameDependency version=...>`: minimum game version.
    Game { version: String },
    /// `<foseDependency version=...>`: minimum script-extender version.
    ScriptExtender { version: String },
    /// `<fommDependency version=...>`: mod-manager version; lmm treats it
    /// as satisfied (we are not FOMM) but records it for validate output.
    ModManager { version: String },
    /// Nested `<dependencies>`.
    Nested(Composite),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum FileState {
    Active,
    Inactive,
    Missing,
}
