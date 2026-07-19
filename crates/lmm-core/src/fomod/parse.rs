//! Streaming FOMOD XML parsing with hard limits.
//!
//! Interpretation rules, chosen to match how mainstream managers (MO2,
//! Vortex) read the format in practice:
//!
//! * Element and attribute names are matched ASCII-case-insensitively —
//!   the schema is camelCase but real archives are sloppy.
//! * Unknown elements are skipped with a warning (tolerant), but anything
//!   that would change *which files get installed* if misread — unknown
//!   dependency operators, file states, malformed mappings — is an error.
//! * `order` defaults to `Ascending` (the schema default), so unordered
//!   steps/groups/plugins are sorted by name; `Explicit` preserves document
//!   order.
//! * `<!DOCTYPE` is rejected outright: no DTDs means no entity expansion
//!   tricks and no external references. quick-xml never fetches anything,
//!   this just refuses the input earlier and louder.
//!
//! Limits (size, depth, element counts, text lengths) are enforced while
//! parsing so a hostile archive cannot balloon memory; see [`Limits`].

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};

use super::model::*;
use crate::error::{Error, Result};

/// Caps applied to untrusted installer XML. Fixed rather than configurable:
/// they are far above anything a legitimate installer needs.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_xml_bytes: u64,
    pub max_depth: usize,
    pub max_steps: usize,
    pub max_groups: usize,
    pub max_options: usize,
    pub max_mappings: usize,
    pub max_flags: usize,
    /// Descriptions longer than this are truncated (with a warning).
    pub max_text_len: usize,
}

pub const LIMITS: Limits = Limits {
    max_xml_bytes: 8 * 1024 * 1024,
    max_depth: 64,
    max_steps: 200,
    max_groups: 1_000,
    max_options: 10_000,
    max_mappings: 50_000,
    max_flags: 5_000,
    max_text_len: 16 * 1024,
};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::Fomod(format!("invalid ModuleConfig.xml: {msg}"))
}

/// Parse `fomod/info.xml`. Tolerant to a fault: this file is display-only
/// metadata, so any structural surprise degrades to an empty field rather
/// than failing the install.
pub fn parse_info(xml: &str) -> Result<ModuleInfo> {
    let mut p = P::new(xml)?;
    let mut info = ModuleInfo::default();
    let Some(root) = p.next_element()? else {
        return Ok(info);
    };
    if root.empty {
        return Ok(info);
    }
    p.children(&root, |p, el| {
        let text = if el.empty {
            String::new()
        } else {
            p.text(&el)?
        };
        let text = (!text.is_empty()).then_some(text);
        match el.name.as_str() {
            "name" => info.name = text,
            "author" => info.author = text,
            "version" => info.version = text,
            "description" => info.description = text,
            "website" => info.website = text,
            _ => {} // info.xml regularly carries tool-specific extras
        }
        Ok(())
    })?;
    Ok(info)
}

/// Parse `fomod/ModuleConfig.xml` into a [`Module`].
pub fn parse_module_config(xml: &str) -> Result<Module> {
    let mut p = P::new(xml)?;
    let root = p.next_element()?.ok_or_else(|| bad("no root element"))?;
    if root.name != "config" {
        return Err(bad(format!(
            "root element is <{}>, expected <config>",
            root.name
        )));
    }

    let mut module = Module {
        name: String::new(),
        image: None,
        module_dependencies: None,
        required_files: Vec::new(),
        steps: Vec::new(),
        conditional_installs: Vec::new(),
        warnings: Vec::new(),
    };

    p.children(&root, |p, el| {
        match el.name.as_str() {
            "modulename" => module.name = p.text(&el)?,
            "moduleimage" => {
                module.image = el.attr("path")?;
                p.skip(&el)?;
            }
            "moduledependencies" => {
                module.module_dependencies = Some(p.composite(&el)?);
            }
            "requiredinstallfiles" => module.required_files = p.mappings(&el)?,
            "installsteps" => {
                let order = el.order()?;
                p.children(&el, |p, el| {
                    if el.name != "installstep" {
                        p.unknown(&el)?;
                        return Ok(());
                    }
                    p.count_steps += 1;
                    if p.count_steps > LIMITS.max_steps {
                        return Err(bad(format!("more than {} install steps", LIMITS.max_steps)));
                    }
                    let step = p.step(&el)?;
                    module.steps.push(step);
                    Ok(())
                })?;
                sort_by_order(&mut module.steps, order, |s| &s.name);
            }
            "conditionalfileinstalls" => {
                module.conditional_installs = p.conditional_installs(&el)?;
            }
            _ => p.unknown(&el)?,
        }
        Ok(())
    })?;

    if module.name.is_empty() {
        module.name = "(unnamed module)".into();
    }
    module.warnings = std::mem::take(&mut p.warnings);
    Ok(module)
}

/// Stable-sort per the FOMOD `order` attribute; `Explicit` keeps document
/// order. Sorting is case-insensitive by display name, like the reference
/// implementations.
fn sort_by_order<T>(items: &mut [T], order: OrderAttr, name: impl Fn(&T) -> &str) {
    match order {
        OrderAttr::Explicit => {}
        OrderAttr::Ascending => items.sort_by_key(|i| name(i).to_lowercase()),
        OrderAttr::Descending => {
            items.sort_by_key(|i| name(i).to_lowercase());
            items.reverse();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrderAttr {
    Explicit,
    Ascending,
    Descending,
}

/// One opened element: lowercased local name plus its raw start tag (for
/// attribute access) and whether it was self-closing.
struct Elem<'a> {
    name: String,
    start: BytesStart<'a>,
    empty: bool,
}

impl Elem<'_> {
    /// Attribute value by ASCII-case-insensitive name, entity-unescaped.
    fn attr(&self, name: &str) -> Result<Option<String>> {
        for a in self.start.attributes() {
            let a = a.map_err(|e| bad(format!("bad attribute in <{}>: {e}", self.name)))?;
            if a.key
                .local_name()
                .as_ref()
                .eq_ignore_ascii_case(name.as_bytes())
            {
                let v = a
                    .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                    .map_err(|e| bad(format!("bad attribute value in <{}>: {e}", self.name)))?;
                return Ok(Some(v.into_owned()));
            }
        }
        Ok(None)
    }

    fn attr_required(&self, name: &str) -> Result<String> {
        self.attr(name)?.ok_or_else(|| {
            bad(format!(
                "<{}> is missing required attribute '{name}'",
                self.name
            ))
        })
    }

    /// The `order` attribute; the schema default is Ascending.
    fn order(&self) -> Result<OrderAttr> {
        Ok(match self.attr("order")?.as_deref() {
            None => OrderAttr::Ascending,
            Some(o) if o.eq_ignore_ascii_case("explicit") => OrderAttr::Explicit,
            Some(o) if o.eq_ignore_ascii_case("ascending") => OrderAttr::Ascending,
            Some(o) if o.eq_ignore_ascii_case("descending") => OrderAttr::Descending,
            // An unknown order would silently reorder options: refuse.
            Some(o) => return Err(bad(format!("unknown order '{o}'"))),
        })
    }
}

/// Parser state: the reader plus limit counters and collected warnings.
struct P<'a> {
    r: Reader<&'a [u8]>,
    warnings: Vec<String>,
    depth: usize,
    count_steps: usize,
    count_groups: usize,
    count_options: usize,
    count_mappings: usize,
    count_flags: usize,
}

impl<'a> P<'a> {
    fn new(xml: &'a str) -> Result<P<'a>> {
        if xml.len() as u64 > LIMITS.max_xml_bytes {
            return Err(bad(format!(
                "XML larger than {} MiB",
                LIMITS.max_xml_bytes / (1024 * 1024)
            )));
        }
        // No trim_text: it would eat significant whitespace around entity
        // references in mixed content; text() trims the assembled result.
        let r = Reader::from_reader(xml.as_bytes());
        Ok(P {
            r,
            warnings: Vec::new(),
            depth: 0,
            count_steps: 0,
            count_groups: 0,
            count_options: 0,
            count_mappings: 0,
            count_flags: 0,
        })
    }

    fn warn(&mut self, msg: impl Into<String>) {
        // Cap: a hostile file must not balloon memory through warnings.
        if self.warnings.len() < 100 {
            self.warnings.push(msg.into());
        }
    }

    fn read(&mut self) -> Result<Event<'a>> {
        loop {
            match self.r.read_event().map_err(bad)? {
                // DTDs are the vector for entity-expansion and external
                // references; installer XML has no legitimate use for one.
                Event::DocType(_) => {
                    return Err(Error::UnsafeArchive(
                        "ModuleConfig.xml contains a DOCTYPE declaration".into(),
                    ));
                }
                Event::Comment(_) | Event::PI(_) | Event::Decl(_) => continue,
                ev => return Ok(ev),
            }
        }
    }

    /// Next element at the current level, or None at the enclosing End/Eof.
    /// Text content encountered where elements are expected is ignored.
    fn next_element(&mut self) -> Result<Option<Elem<'a>>> {
        loop {
            match self.read()? {
                Event::Start(s) => {
                    self.depth += 1;
                    if self.depth > LIMITS.max_depth {
                        return Err(bad(format!("nested deeper than {}", LIMITS.max_depth)));
                    }
                    return Ok(Some(Elem {
                        name: local_lower(&s),
                        start: s,
                        empty: false,
                    }));
                }
                Event::Empty(s) => {
                    return Ok(Some(Elem {
                        name: local_lower(&s),
                        start: s,
                        empty: true,
                    }));
                }
                Event::End(_) => {
                    self.depth = self.depth.saturating_sub(1);
                    return Ok(None);
                }
                Event::Eof => return Ok(None),
                Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) => continue,
                Event::Comment(_) | Event::PI(_) | Event::Decl(_) | Event::DocType(_) => {
                    unreachable!("filtered by read()")
                }
            }
        }
    }

    /// Visit every child element of `el` (does nothing for self-closing
    /// tags). The callback must fully consume each child it receives,
    /// either by parsing it or via [`P::skip`]/[`P::unknown`].
    fn children(
        &mut self,
        el: &Elem<'a>,
        mut f: impl FnMut(&mut Self, Elem<'a>) -> Result<()>,
    ) -> Result<()> {
        if el.empty {
            return Ok(());
        }
        while let Some(child) = self.next_element()? {
            f(self, child)?;
        }
        Ok(())
    }

    /// Consume and discard the rest of `el`'s subtree.
    fn skip(&mut self, el: &Elem<'a>) -> Result<()> {
        if el.empty {
            return Ok(());
        }
        let mut depth = 1usize;
        loop {
            match self.read()? {
                Event::Start(_) => {
                    depth += 1;
                    if self.depth + depth > LIMITS.max_depth {
                        return Err(bad(format!("nested deeper than {}", LIMITS.max_depth)));
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        self.depth = self.depth.saturating_sub(1);
                        return Ok(());
                    }
                }
                Event::Eof => return Err(bad(format!("unclosed <{}>", el.name))),
                _ => {}
            }
        }
    }

    /// Skip an element we don't understand, noting it once.
    fn unknown(&mut self, el: &Elem<'a>) -> Result<()> {
        self.warn(format!("ignored unknown element <{}>", el.name));
        self.skip(el)
    }

    /// Collect the text content of `el` (Text/CData/character references),
    /// skipping unexpected child elements with a warning. Truncated at the
    /// text limit.
    fn text(&mut self, el: &Elem<'a>) -> Result<String> {
        if el.empty {
            return Ok(String::new());
        }
        let mut out = String::new();
        let mut truncated = false;
        let mut push = |s: &str, truncated: &mut bool| {
            let room = LIMITS.max_text_len.saturating_sub(out.len());
            if s.len() <= room {
                out.push_str(s);
            } else {
                // Truncate on a char boundary within the room we have.
                let mut end = room;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                out.push_str(&s[..end]);
                *truncated = true;
            }
        };
        let mut depth = 1usize;
        loop {
            match self.read()? {
                Event::Text(t) => {
                    let s = t.decode().map_err(|e| bad(format!("bad text: {e}")))?;
                    if depth == 1 {
                        push(&s, &mut truncated);
                    }
                }
                Event::CData(c) => {
                    let s = c.decode().map_err(|e| bad(format!("bad CDATA: {e}")))?;
                    if depth == 1 {
                        push(&s, &mut truncated);
                    }
                }
                Event::GeneralRef(r) => {
                    // Only predefined entities and character references
                    // resolve; anything else would need a DTD, which is
                    // rejected above.
                    let name = r.decode().map_err(|e| bad(format!("bad reference: {e}")))?;
                    let resolved = match r.resolve_char_ref() {
                        Ok(Some(c)) => c.to_string(),
                        _ => quick_xml::escape::resolve_predefined_entity(&name)
                            .ok_or_else(|| bad(format!("unresolvable entity '&{name};'")))?
                            .to_string(),
                    };
                    if depth == 1 {
                        push(&resolved, &mut truncated);
                    }
                }
                Event::Start(s) => {
                    depth += 1;
                    if depth == 2 {
                        self.warn(format!(
                            "ignored element <{}> inside <{}> text",
                            local_lower(&s),
                            el.name
                        ));
                    }
                }
                Event::End(_) => {
                    depth -= 1;
                    if depth == 0 {
                        self.depth = self.depth.saturating_sub(1);
                        break;
                    }
                }
                Event::Eof => return Err(bad(format!("unclosed <{}>", el.name))),
                _ => {}
            }
        }
        if truncated {
            self.warn(format!(
                "text of <{}> truncated to {} bytes",
                el.name, LIMITS.max_text_len
            ));
        }
        Ok(out.trim().to_string())
    }

    // -- FOMOD grammar ----------------------------------------------------

    fn step(&mut self, el: &Elem<'a>) -> Result<Step> {
        let mut step = Step {
            name: el.attr("name")?.unwrap_or_else(|| "Install".into()),
            visible: None,
            groups: Vec::new(),
        };
        self.children(el, |p, child| {
            match child.name.as_str() {
                "visible" => step.visible = Some(p.composite(&child)?),
                "optionalfilegroups" => {
                    let order = child.order()?;
                    p.children(&child, |p, g| {
                        if g.name != "group" {
                            p.unknown(&g)?;
                            return Ok(());
                        }
                        p.count_groups += 1;
                        if p.count_groups > LIMITS.max_groups {
                            return Err(bad(format!("more than {} groups", LIMITS.max_groups)));
                        }
                        let group = p.group(&g)?;
                        step.groups.push(group);
                        Ok(())
                    })?;
                    sort_by_order(&mut step.groups, order, |g| &g.name);
                }
                _ => p.unknown(&child)?,
            }
            Ok(())
        })?;
        Ok(step)
    }

    fn group(&mut self, el: &Elem<'a>) -> Result<Group> {
        let name = el.attr("name")?.unwrap_or_else(|| "Options".into());
        let rule = match el.attr("type")?.as_deref() {
            Some(t) if t.eq_ignore_ascii_case("selectexactlyone") => GroupRule::ExactlyOne,
            Some(t) if t.eq_ignore_ascii_case("selectatmostone") => GroupRule::AtMostOne,
            Some(t) if t.eq_ignore_ascii_case("selectatleastone") => GroupRule::AtLeastOne,
            Some(t) if t.eq_ignore_ascii_case("selectany") => GroupRule::Any,
            Some(t) if t.eq_ignore_ascii_case("selectall") => GroupRule::All,
            Some(t) => {
                // The user still chooses interactively, so a lax fallback
                // cannot install unchosen files — but say so.
                self.warn(format!(
                    "group '{name}': unknown type '{t}', treating as SelectAny"
                ));
                GroupRule::Any
            }
            None => {
                self.warn(format!(
                    "group '{name}': missing type, treating as SelectAny"
                ));
                GroupRule::Any
            }
        };
        let mut group = Group {
            name,
            rule,
            options: Vec::new(),
        };
        self.children(el, |p, child| {
            match child.name.as_str() {
                "plugins" => {
                    let order = child.order()?;
                    p.children(&child, |p, pl| {
                        if pl.name != "plugin" {
                            p.unknown(&pl)?;
                            return Ok(());
                        }
                        p.count_options += 1;
                        if p.count_options > LIMITS.max_options {
                            return Err(bad(format!("more than {} options", LIMITS.max_options)));
                        }
                        let opt = p.plugin(&pl)?;
                        group.options.push(opt);
                        Ok(())
                    })?;
                    sort_by_order(&mut group.options, order, |o| &o.name);
                }
                _ => p.unknown(&child)?,
            }
            Ok(())
        })?;
        Ok(group)
    }

    fn plugin(&mut self, el: &Elem<'a>) -> Result<OptionDef> {
        let mut opt = OptionDef {
            name: el.attr_required("name")?,
            description: String::new(),
            image: None,
            files: Vec::new(),
            flags: Vec::new(),
            // The schema requires a typeDescriptor; missing means Optional.
            type_desc: TypeDescriptor::Simple(OptionType::Optional),
        };
        self.children(el, |p, child| {
            match child.name.as_str() {
                "description" => opt.description = p.text(&child)?,
                "image" => {
                    opt.image = child.attr("path")?;
                    p.skip(&child)?;
                }
                "files" => opt.files = p.mappings(&child)?,
                "conditionflags" => {
                    p.children(&child, |p, f| {
                        if f.name != "flag" {
                            p.unknown(&f)?;
                            return Ok(());
                        }
                        p.count_flags += 1;
                        if p.count_flags > LIMITS.max_flags {
                            return Err(bad(format!("more than {} flags", LIMITS.max_flags)));
                        }
                        let name = f.attr_required("name")?;
                        let value = p.text(&f)?;
                        opt.flags.push(FlagSet { name, value });
                        Ok(())
                    })?;
                }
                "typedescriptor" => opt.type_desc = p.type_descriptor(&child)?,
                _ => p.unknown(&child)?,
            }
            Ok(())
        })?;
        Ok(opt)
    }

    fn type_descriptor(&mut self, el: &Elem<'a>) -> Result<TypeDescriptor> {
        let mut result = TypeDescriptor::Simple(OptionType::Optional);
        self.children(el, |p, child| {
            match child.name.as_str() {
                "type" => {
                    result = TypeDescriptor::Simple(p.option_type(&child)?);
                    p.skip(&child)?;
                }
                "dependencytype" => {
                    let mut default = OptionType::Optional;
                    let mut patterns = Vec::new();
                    p.children(&child, |p, c| {
                        match c.name.as_str() {
                            "defaulttype" => {
                                default = p.option_type(&c)?;
                                p.skip(&c)?;
                            }
                            "patterns" => {
                                p.children(&c, |p, pat| {
                                    if pat.name != "pattern" {
                                        p.unknown(&pat)?;
                                        return Ok(());
                                    }
                                    let tp = p.type_pattern(&pat)?;
                                    patterns.push(tp);
                                    Ok(())
                                })?;
                            }
                            _ => p.unknown(&c)?,
                        }
                        Ok(())
                    })?;
                    result = TypeDescriptor::Dependent { default, patterns };
                }
                _ => p.unknown(&child)?,
            }
            Ok(())
        })?;
        Ok(result)
    }

    fn type_pattern(&mut self, el: &Elem<'a>) -> Result<TypePattern> {
        let mut when = None;
        let mut becomes = OptionType::Optional;
        self.children(el, |p, child| {
            match child.name.as_str() {
                "dependencies" => when = Some(p.composite(&child)?),
                "type" => {
                    becomes = p.option_type(&child)?;
                    p.skip(&child)?;
                }
                _ => p.unknown(&child)?,
            }
            Ok(())
        })?;
        Ok(TypePattern {
            when: when.ok_or_else(|| bad("type <pattern> without <dependencies>"))?,
            becomes,
        })
    }

    fn option_type(&mut self, el: &Elem<'a>) -> Result<OptionType> {
        let name = el.attr_required("name")?;
        Ok(match name.to_ascii_lowercase().as_str() {
            "required" => OptionType::Required,
            "recommended" => OptionType::Recommended,
            "optional" => OptionType::Optional,
            "notusable" => OptionType::NotUsable,
            "couldbeusable" => OptionType::CouldBeUsable,
            other => {
                // A misread type could lock or unlock the wrong option, but
                // the user still sees and confirms every selection.
                self.warn(format!(
                    "unknown plugin type '{other}', treating as Optional"
                ));
                OptionType::Optional
            }
        })
    }

    /// `<conditionalFileInstalls>` → `<patterns>` → `<pattern>` list.
    fn conditional_installs(&mut self, el: &Elem<'a>) -> Result<Vec<ConditionalInstall>> {
        let mut out = Vec::new();
        self.children(el, |p, child| {
            if child.name != "patterns" {
                p.unknown(&child)?;
                return Ok(());
            }
            p.children(&child, |p, pat| {
                if pat.name != "pattern" {
                    p.unknown(&pat)?;
                    return Ok(());
                }
                let mut when = None;
                let mut files = Vec::new();
                p.children(&pat, |p, c| {
                    match c.name.as_str() {
                        "dependencies" => when = Some(p.composite(&c)?),
                        "files" => files = p.mappings(&c)?,
                        _ => p.unknown(&c)?,
                    }
                    Ok(())
                })?;
                out.push(ConditionalInstall {
                    when: when.ok_or_else(|| {
                        bad("conditional install <pattern> without <dependencies>")
                    })?,
                    files,
                });
                Ok(())
            })?;
            Ok(())
        })?;
        Ok(out)
    }

    /// A `<files>`/`<requiredInstallFiles>` list of file/folder mappings.
    fn mappings(&mut self, el: &Elem<'a>) -> Result<Vec<Mapping>> {
        let mut out = Vec::new();
        self.children(el, |p, child| {
            let is_folder = match child.name.as_str() {
                "file" => false,
                "folder" => true,
                _ => {
                    p.unknown(&child)?;
                    return Ok(());
                }
            };
            p.count_mappings += 1;
            if p.count_mappings > LIMITS.max_mappings {
                return Err(bad(format!(
                    "more than {} file mappings",
                    LIMITS.max_mappings
                )));
            }
            let source = child.attr_required("source")?;
            let dest = child.attr("destination")?;
            let priority = match child.attr("priority")? {
                Some(s) => s.trim().parse::<i64>().map_err(|_| {
                    bad(format!(
                        "mapping '{source}': priority '{s}' is not a number"
                    ))
                })?,
                None => 0,
            };
            let always_install = bool_attr(&child, "alwaysinstall")?;
            let install_if_usable = bool_attr(&child, "installifusable")?;
            p.skip(&child)?;
            out.push(Mapping {
                source,
                dest,
                is_folder,
                priority,
                always_install,
                install_if_usable,
            });
            Ok(())
        })?;
        Ok(out)
    }

    /// `<dependencies>` / `<visible>` / `<moduleDependencies>`: an operator
    /// plus leaf conditions and nested composites.
    fn composite(&mut self, el: &Elem<'a>) -> Result<Composite> {
        let op = match el.attr("operator")?.as_deref() {
            None => Op::And,
            Some(o) if o.eq_ignore_ascii_case("and") => Op::And,
            Some(o) if o.eq_ignore_ascii_case("or") => Op::Or,
            // Misreading the operator would materially change evaluation.
            Some(o) => return Err(bad(format!("unknown dependency operator '{o}'"))),
        };
        let mut parts = Vec::new();
        self.children(el, |p, child| {
            match child.name.as_str() {
                "filedependency" => {
                    let file = child.attr_required("file")?;
                    let state = match child.attr("state")?.as_deref() {
                        None => FileState::Active,
                        Some(s) if s.eq_ignore_ascii_case("active") => FileState::Active,
                        Some(s) if s.eq_ignore_ascii_case("inactive") => FileState::Inactive,
                        Some(s) if s.eq_ignore_ascii_case("missing") => FileState::Missing,
                        Some(s) => return Err(bad(format!("unknown file state '{s}'"))),
                    };
                    p.skip(&child)?;
                    parts.push(Condition::File { file, state });
                }
                "flagdependency" => {
                    let flag = child.attr_required("flag")?;
                    let value = child.attr("value")?.unwrap_or_default();
                    p.skip(&child)?;
                    parts.push(Condition::Flag { flag, value });
                }
                "gamedependency" => {
                    let version = child.attr_required("version")?;
                    p.skip(&child)?;
                    parts.push(Condition::Game { version });
                }
                "fosedependency" => {
                    let version = child.attr_required("version")?;
                    p.skip(&child)?;
                    parts.push(Condition::ScriptExtender { version });
                }
                "fommdependency" => {
                    let version = child.attr_required("version")?;
                    p.skip(&child)?;
                    parts.push(Condition::ModManager { version });
                }
                "dependencies" => {
                    let nested = p.composite(&child)?;
                    parts.push(Condition::Nested(nested));
                }
                _ => p.unknown(&child)?,
            }
            Ok(())
        })?;
        Ok(Composite { op, parts })
    }
}

fn bool_attr(el: &Elem<'_>, name: &str) -> Result<bool> {
    Ok(match el.attr(name)?.as_deref() {
        None => false,
        Some(v) if v.eq_ignore_ascii_case("true") || v == "1" => true,
        Some(v) if v.eq_ignore_ascii_case("false") || v == "0" => false,
        Some(v) => return Err(bad(format!("attribute {name}='{v}' is not a boolean"))),
    })
}

fn local_lower(s: &BytesStart<'_>) -> String {
    String::from_utf8_lossy(s.local_name().as_ref()).to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small but structurally complete installer.
    const BASIC: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<config xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance">
  <moduleName>Example &amp; Mod</moduleName>
  <moduleImage path="fomod/header.png"/>
  <requiredInstallFiles>
    <file source="core/base.esp" destination="base.esp"/>
    <folder source="core/textures" destination="textures" priority="1"/>
  </requiredInstallFiles>
  <installSteps order="Explicit">
    <installStep name="Textures">
      <optionalFileGroups order="Explicit">
        <group name="Resolution" type="SelectExactlyOne">
          <plugins order="Explicit">
            <plugin name="High">
              <description>Big &lt;textures&gt;</description>
              <image path="fomod/high.png"/>
              <files><folder source="options/high" destination=""/></files>
              <conditionFlags><flag name="res">high</flag></conditionFlags>
              <typeDescriptor><type name="Recommended"/></typeDescriptor>
            </plugin>
            <plugin name="Low">
              <description>Small</description>
              <files><folder source="options/low"/></files>
              <typeDescriptor><type name="Optional"/></typeDescriptor>
            </plugin>
          </plugins>
        </group>
      </optionalFileGroups>
    </installStep>
    <installStep name="Patches">
      <visible><flagDependency flag="res" value="high"/></visible>
      <optionalFileGroups>
        <group name="Extras" type="SelectAny">
          <plugins>
            <plugin name="ENB Patch">
              <description/>
              <files><file source="patches/enb.esp"/></files>
              <typeDescriptor>
                <dependencyType>
                  <defaultType name="Optional"/>
                  <patterns>
                    <pattern>
                      <dependencies operator="And">
                        <fileDependency file="enbseries.ini" state="Active"/>
                      </dependencies>
                      <type name="Recommended"/>
                    </pattern>
                  </patterns>
                </dependencyType>
              </typeDescriptor>
            </plugin>
          </plugins>
        </group>
      </optionalFileGroups>
    </installStep>
  </installSteps>
  <conditionalFileInstalls>
    <patterns>
      <pattern>
        <dependencies operator="Or">
          <flagDependency flag="res" value="high"/>
          <dependencies operator="And">
            <gameDependency version="1.5.97"/>
            <foseDependency version="2.0.20"/>
          </dependencies>
        </dependencies>
        <files><file source="extra/hd.esp" destination="hd.esp" priority="2"/></files>
      </pattern>
    </patterns>
  </conditionalFileInstalls>
</config>"#;

    #[test]
    fn parses_a_complete_module() {
        let m = parse_module_config(BASIC).unwrap();
        assert_eq!(m.name, "Example & Mod");
        assert_eq!(m.image.as_deref(), Some("fomod/header.png"));
        assert_eq!(m.required_files.len(), 2);
        assert!(m.required_files[1].is_folder);
        assert_eq!(m.required_files[1].priority, 1);

        assert_eq!(m.steps.len(), 2);
        let s1 = &m.steps[0];
        assert_eq!(s1.name, "Textures");
        assert!(s1.visible.is_none());
        assert_eq!(s1.groups[0].rule, GroupRule::ExactlyOne);
        let high = &s1.groups[0].options[0];
        assert_eq!(high.name, "High");
        assert_eq!(high.description, "Big <textures>");
        assert_eq!(
            high.flags,
            vec![FlagSet {
                name: "res".into(),
                value: "high".into()
            }]
        );
        assert!(matches!(
            high.type_desc,
            TypeDescriptor::Simple(OptionType::Recommended)
        ));
        // Folder with empty destination maps onto the mod root.
        assert_eq!(high.files[0].dest.as_deref(), Some(""));
        // Missing destination stays None (mirrors source later).
        assert_eq!(s1.groups[0].options[1].files[0].dest, None);

        let s2 = &m.steps[1];
        let vis = s2.visible.as_ref().unwrap();
        assert_eq!(vis.op, Op::And);
        assert_eq!(
            vis.parts[0],
            Condition::Flag {
                flag: "res".into(),
                value: "high".into()
            }
        );
        match &s2.groups[0].options[0].type_desc {
            TypeDescriptor::Dependent { default, patterns } => {
                assert_eq!(*default, OptionType::Optional);
                assert_eq!(patterns[0].becomes, OptionType::Recommended);
                assert_eq!(
                    patterns[0].when.parts[0],
                    Condition::File {
                        file: "enbseries.ini".into(),
                        state: FileState::Active
                    }
                );
            }
            other => panic!("expected dependency type, got {other:?}"),
        }

        let ci = &m.conditional_installs[0];
        assert_eq!(ci.when.op, Op::Or);
        assert_eq!(ci.when.parts.len(), 2);
        match &ci.when.parts[1] {
            Condition::Nested(inner) => {
                assert_eq!(inner.op, Op::And);
                assert_eq!(inner.parts.len(), 2);
            }
            other => panic!("expected nested dependencies, got {other:?}"),
        }
        assert_eq!(ci.files[0].priority, 2);
        assert!(m.warnings.is_empty(), "{:?}", m.warnings);
    }

    #[test]
    fn default_order_is_ascending() {
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps>
            <installStep name="Zeta"><optionalFileGroups/></installStep>
            <installStep name="alpha"><optionalFileGroups/></installStep>
          </installSteps></config>"#;
        let m = parse_module_config(xml).unwrap();
        assert_eq!(m.steps[0].name, "alpha");
        assert_eq!(m.steps[1].name, "Zeta");
    }

    #[test]
    fn explicit_order_keeps_document_order() {
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps order="Explicit">
            <installStep name="Zeta"><optionalFileGroups/></installStep>
            <installStep name="alpha"><optionalFileGroups/></installStep>
          </installSteps></config>"#;
        let m = parse_module_config(xml).unwrap();
        assert_eq!(m.steps[0].name, "Zeta");
    }

    #[test]
    fn element_names_match_case_insensitively() {
        let xml = r#"<Config><ModuleName>M</ModuleName>
          <RequiredInstallFiles><File Source="a.esp"/></RequiredInstallFiles></Config>"#;
        let m = parse_module_config(xml).unwrap();
        assert_eq!(m.name, "M");
        assert_eq!(m.required_files[0].source, "a.esp");
    }

    #[test]
    fn unknown_elements_warn_but_parse() {
        let xml = r#"<config><moduleName>m</moduleName>
          <shinyNewThing><child/></shinyNewThing></config>"#;
        let m = parse_module_config(xml).unwrap();
        assert_eq!(m.warnings.len(), 1);
        assert!(m.warnings[0].contains("shinynewthing"));
    }

    #[test]
    fn doctype_is_rejected() {
        let xml = "<!DOCTYPE config [<!ENTITY x \"y\">]><config/>";
        let err = parse_module_config(xml).unwrap_err();
        assert!(matches!(err, Error::UnsafeArchive(_)), "{err}");
    }

    #[test]
    fn material_mistakes_are_errors() {
        // Unknown dependency operator.
        let xml = r#"<config><moduleName>m</moduleName>
          <moduleDependencies operator="Xor"/></config>"#;
        assert!(parse_module_config(xml).is_err());
        // Unknown file state.
        let xml = r#"<config><moduleName>m</moduleName>
          <moduleDependencies><fileDependency file="a" state="Sideways"/></moduleDependencies>
          </config>"#;
        assert!(parse_module_config(xml).is_err());
        // Mapping without source.
        let xml = r#"<config><moduleName>m</moduleName>
          <requiredInstallFiles><file destination="a"/></requiredInstallFiles></config>"#;
        assert!(parse_module_config(xml).is_err());
        // Non-numeric priority.
        let xml = r#"<config><moduleName>m</moduleName>
          <requiredInstallFiles><file source="a" priority="high"/></requiredInstallFiles></config>"#;
        assert!(parse_module_config(xml).is_err());
        // Unknown order.
        let xml = r#"<config><moduleName>m</moduleName>
          <installSteps order="Random"/></config>"#;
        assert!(parse_module_config(xml).is_err());
    }

    #[test]
    fn truncated_xml_is_an_error() {
        assert!(parse_module_config("<config><moduleName>m").is_err());
        assert!(parse_module_config("not xml at all").is_err());
        assert!(parse_module_config("").is_err());
    }

    #[test]
    fn step_count_limit_applies() {
        let mut xml =
            String::from("<config><moduleName>m</moduleName><installSteps order=\"Explicit\">");
        for i in 0..=LIMITS.max_steps {
            xml.push_str(&format!(
                "<installStep name=\"s{i}\"><optionalFileGroups/></installStep>"
            ));
        }
        xml.push_str("</installSteps></config>");
        let err = parse_module_config(&xml).unwrap_err();
        assert!(err.to_string().contains("install steps"), "{err}");
    }

    #[test]
    fn info_xml_parses_and_tolerates_extras() {
        let info = parse_info(
            r#"<fomod><Name>Mod</Name><Author>A</Author><Version MachineVersion="1.2">1.2</Version>
               <SomethingElse>x</SomethingElse></fomod>"#,
        )
        .unwrap();
        assert_eq!(info.name.as_deref(), Some("Mod"));
        assert_eq!(info.author.as_deref(), Some("A"));
        assert_eq!(info.version.as_deref(), Some("1.2"));
        assert_eq!(info.description, None);
    }

    #[test]
    fn boolean_attributes() {
        let xml = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
          <file source="a" alwaysInstall="true" installIfUsable="0"/>
          </requiredInstallFiles></config>"#;
        let m = parse_module_config(xml).unwrap();
        assert!(m.required_files[0].always_install);
        assert!(!m.required_files[0].install_if_usable);
        let xml = r#"<config><moduleName>m</moduleName><requiredInstallFiles>
          <file source="a" alwaysInstall="maybe"/></requiredInstallFiles></config>"#;
        assert!(parse_module_config(xml).is_err());
    }
}
