//! The completion engine: turns (line, cursor) into ranked candidates.
//!
//! Pipeline, all pure except the [`Snapshot`] lookup:
//!
//! ```text
//! line + pos ──tokenize──► tokens before cursor + current word (+ quoting)
//!            ──resolve───► what the word is: command / subcommand / flag /
//!                          a positional of kind X (args::positional_kind)
//!            ──candidates► static (from the clap definition) or dynamic
//!                          (from the data snapshot) or filesystem (paths)
//!            ──rank──────► tiered matching, best first
//!            ──quote─────► insertions escaped with the same rules the
//!                          parser uses (shlex), so a completed value can
//!                          never split into multiple arguments
//! ```
//!
//! The engine is deliberately editor-agnostic: it returns plain
//! [`Completions`], and thin adapters in `shell/mod.rs` translate those to
//! reedline suggestions and inline hints. That keeps everything here
//! testable without a terminal.

use clap::CommandFactory;

use crate::args::{self, Args, CompletionKind, DownloadFilter};
use crate::shell::data::Snapshot;

/// Shell-only words that are not clap commands but are always valid.
const EXTRA_COMMANDS: &[&str] = &["q", "quit", "exit"];

// ---------------------------------------------------------------------------
// Tokenizing

/// The parsed prefix of a line up to the cursor.
#[derive(Debug, PartialEq, Eq)]
pub struct LinePrefix {
    /// Completed tokens before the word being completed, unquoted.
    pub tokens: Vec<String>,
    /// The (partial, unquoted) word at the cursor.
    pub word: String,
    /// Byte offset where the current word starts — including its opening
    /// quote if any, so replacing `line[word_start..pos]` swaps the whole
    /// argument.
    pub word_start: usize,
    /// The quote character the current word was opened with, if unclosed.
    pub open_quote: Option<char>,
}

/// Split `line[..pos]` with the same quoting rules the parser (shlex) uses:
/// single quotes are literal, double quotes allow backslash escapes, an
/// unterminated quote extends to the cursor. Completion must understand a
/// half-typed `disable "Unoff` exactly like the parser will once it is
/// finished.
pub fn tokenize(line: &str, pos: usize) -> LinePrefix {
    let text = &line[..pos.min(line.len())];
    let mut tokens = Vec::new();
    let mut word = String::new();
    let mut word_start = 0;
    let mut in_word = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for (i, c) in text.char_indices() {
        if !in_word {
            if c.is_whitespace() {
                continue;
            }
            in_word = true;
            word_start = i;
        }
        if escaped {
            escaped = false;
            word.push(c);
            continue;
        }
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some('"') | None, '\\') => escaped = true,
            (Some(_), _) => word.push(c),
            (None, '\'' | '"') => quote = Some(c),
            (None, _) if c.is_whitespace() => {
                tokens.push(std::mem::take(&mut word));
                in_word = false;
            }
            (None, _) => word.push(c),
        }
    }
    if !in_word {
        word_start = text.len();
    }
    LinePrefix {
        tokens,
        word,
        word_start,
        open_quote: quote,
    }
}

// ---------------------------------------------------------------------------
// Resolving what to complete

/// What the word under the cursor should be completed as.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// First word: a top-level command name.
    Command,
    /// Subcommand of `parent`.
    Subcommand(String),
    /// A `-`/`--` option of the command at `path`.
    Flag(Vec<String>),
    /// Positional argument: kind from [`args::positional_kind`].
    Positional(CompletionKind),
}

/// Walk the tokens before the cursor against the clap command tree.
///
/// Flags are skipped; a flag that takes a value (`--name X`) also swallows
/// the following token, so positional indices stay correct. This uses clap
/// reflection rather than a hardcoded flag list.
fn resolve(tokens: &[String], current_word: &str) -> Target {
    let root = Args::command();

    let Some(first) = tokens.first() else {
        return Target::Command;
    };
    let Some(cmd) = root.find_subcommand(first.as_str()) else {
        // Unknown command: nothing sensible to offer.
        return Target::Positional(CompletionKind::None);
    };

    let mut path = vec![first.clone()];
    let mut cmd = cmd;
    let mut rest = &tokens[1..];

    // Descend one subcommand level if this command has any (our tree is at
    // most two levels deep; a loop keeps it future-proof).
    while cmd.has_subcommands() {
        match rest.first() {
            Some(t) => match cmd.find_subcommand(t.as_str()) {
                Some(sub) => {
                    path.push(t.clone());
                    cmd = sub;
                    rest = &rest[1..];
                }
                // The token in subcommand position isn't one (maybe a typo,
                // maybe a flag): treat as done, no positional knowledge.
                None => return Target::Positional(CompletionKind::None),
            },
            // The *current word* is the subcommand being typed.
            None if !current_word.starts_with('-') => {
                return Target::Subcommand(first.clone());
            }
            None => return Target::Flag(path),
        }
    }

    if current_word.starts_with('-') {
        return Target::Flag(path);
    }

    // Count positionals among the remaining tokens, skipping flags and the
    // values of flags that take one.
    let mut index = 0usize;
    let mut skip_value = false;
    for t in rest {
        if skip_value {
            skip_value = false;
            continue;
        }
        if t.starts_with('-') {
            skip_value = flag_takes_value(cmd, t);
            continue;
        }
        index += 1;
    }
    if skip_value {
        // Cursor word is a flag's value ("--name <cursor>"): free text.
        return Target::Positional(CompletionKind::None);
    }

    let path_refs: Vec<&str> = path.iter().map(String::as_str).collect();
    Target::Positional(args::positional_kind(&path_refs, index))
}

/// Does `--flag` (or `-f`) of `cmd` consume a following value?
fn flag_takes_value(cmd: &clap::Command, token: &str) -> bool {
    // `--flag=value` carries its value inline and consumes nothing after.
    if token.contains('=') {
        return false;
    }
    cmd.get_arguments().any(|a| {
        let matches = token
            .strip_prefix("--")
            .map(|long| Some(long) == a.get_long())
            .or_else(|| {
                let mut short = token.strip_prefix('-')?.chars();
                Some(short.next() == a.get_short() && short.next().is_none())
            })
            .unwrap_or(false);
        matches && a.get_num_args().is_none_or(|n| n.takes_values())
    })
}

// ---------------------------------------------------------------------------
// Matching and ranking

/// Match quality tiers, best first. Candidates are sorted by tier, then
/// alphabetically, so ranking stays predictable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// Candidate starts with the query, byte for byte.
    ExactPrefix,
    /// Case-insensitive prefix.
    CiPrefix,
    /// Prefix after normalization (case, spaces, `-`, `_` ignored), so
    /// "unofficial-patch" completes "Unofficial Patch".
    NormalizedPrefix,
    /// Some word inside the candidate starts with the query ("interface"
    /// matches "SkyUI - Modern Interface").
    WordPrefix,
    /// Query characters appear in order (optional, config-gated).
    Fuzzy,
}

fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Best tier at which `candidate` matches `query`, if any.
/// An empty query matches everything at the lowest precise tier.
pub fn match_tier(candidate: &str, query: &str, fuzzy: bool) -> Option<Tier> {
    if query.is_empty() {
        return Some(Tier::ExactPrefix);
    }
    if candidate.starts_with(query) {
        return Some(Tier::ExactPrefix);
    }
    let (c_lower, q_lower) = (candidate.to_lowercase(), query.to_lowercase());
    if c_lower.starts_with(&q_lower) {
        return Some(Tier::CiPrefix);
    }
    if normalize(candidate).starts_with(&normalize(query)) {
        return Some(Tier::NormalizedPrefix);
    }
    if candidate
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| !w.is_empty() && w.to_lowercase().starts_with(&q_lower))
    {
        return Some(Tier::WordPrefix);
    }
    if fuzzy && is_subsequence(&q_lower, &c_lower) {
        return Some(Tier::Fuzzy);
    }
    None
}

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

// ---------------------------------------------------------------------------
// Quoting

/// Escape a value with the same rules the line parser (shlex) uses, so a
/// completed argument can never split into several. Values that need no
/// quoting are returned unchanged.
pub fn quote(value: &str) -> Option<String> {
    // try_quote only fails on interior NULs, which no real name contains.
    shlex::try_quote(value).ok().map(|q| q.into_owned())
}

// ---------------------------------------------------------------------------
// The engine

/// One ready-to-use completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    /// Text to insert into the buffer (already quoted if needed).
    pub insert: String,
    /// What to show in the menu (the plain value).
    pub display: String,
    /// Short annotation ("enabled", "pending", …).
    pub description: Option<String>,
    /// Append a space after inserting (off for directories).
    pub append_space: bool,
    tier: Tier,
}

/// Result of a completion request.
#[derive(Debug, Default)]
pub struct Completions {
    /// Byte offset of the region to replace (start of the current word,
    /// including any opening quote).
    pub start: usize,
    pub items: Vec<Item>,
}

/// Engine options, lifted from `[shell.autocomplete]` in the config.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub fuzzy: bool,
    pub descriptions: bool,
}

/// Compute completions for `line` with the cursor at byte `pos` (which may
/// be in the middle of the line; everything after the cursor is left alone).
pub fn complete(snapshot: &Snapshot, opts: Options, line: &str, pos: usize) -> Completions {
    let prefix = tokenize(line, pos);
    let target = resolve(&prefix.tokens, &prefix.word);

    // (value, description) pairs before matching.
    let raw: Vec<(String, Option<String>)> = match &target {
        Target::Command => {
            let root = Args::command();
            root.get_subcommands()
                .map(|c| (c.get_name().to_string(), one_line_about(c)))
                .chain(
                    EXTRA_COMMANDS
                        .iter()
                        .map(|w| (w.to_string(), Some("quit the shell".into()))),
                )
                .collect()
        }
        Target::Subcommand(parent) => {
            let root = Args::command();
            match root.find_subcommand(parent) {
                Some(cmd) => cmd
                    .get_subcommands()
                    .map(|c| (c.get_name().to_string(), one_line_about(c)))
                    .collect(),
                None => Vec::new(),
            }
        }
        Target::Flag(path) => flags_of(path),
        Target::Positional(kind) => match kind {
            CompletionKind::Path => {
                return complete_path(&prefix, opts);
            }
            CompletionKind::Game => snapshot
                .installs
                .iter()
                .map(|s| (s.clone(), None))
                .collect(),
            CompletionKind::Profile => snapshot
                .profiles
                .iter()
                .map(|p| (p.clone(), None))
                .collect(),
            CompletionKind::InstalledMod
            | CompletionKind::EnabledMod
            | CompletionKind::DisabledMod => {
                let want = |enabled: bool| match kind {
                    CompletionKind::EnabledMod => enabled,
                    CompletionKind::DisabledMod => !enabled,
                    _ => true,
                };
                snapshot
                    .mods
                    .iter()
                    .filter(|m| want(m.enabled))
                    .map(|m| {
                        let desc = if m.enabled { "enabled" } else { "disabled" };
                        (m.name.clone(), Some(desc.to_string()))
                    })
                    .collect()
            }
            CompletionKind::Download(filter) => {
                use lmm_nexus::queue::Status;
                let want = |s: Status| match filter {
                    DownloadFilter::Startable => matches!(s, Status::Pending | Status::Failed),
                    DownloadFilter::Cancelable => matches!(s, Status::Pending | Status::Active),
                    DownloadFilter::Failed => s == Status::Failed,
                    DownloadFilter::Finished => matches!(s, Status::Completed | Status::Failed),
                    DownloadFilter::Completed => s == Status::Completed,
                };
                snapshot
                    .downloads
                    .iter()
                    .filter(|d| want(d.status))
                    .map(|d| {
                        (
                            d.id.to_string(),
                            Some(format!("{} — {}", d.status, d.label)),
                        )
                    })
                    .collect()
            }
            CompletionKind::Tool => snapshot
                .tools
                .iter()
                .map(|t| (t.id.clone(), Some(t.name.clone())))
                .collect(),
            CompletionKind::None => Vec::new(),
        },
    };

    let mut items: Vec<Item> = raw
        .into_iter()
        .filter_map(|(value, description)| {
            let tier = match_tier(&value, &prefix.word, opts.fuzzy)?;
            let insert = quoted_insert(&value, prefix.open_quote)?;
            Some(Item {
                insert,
                display: value,
                description: description.filter(|_| opts.descriptions),
                append_space: true,
                tier,
            })
        })
        .collect();
    items.sort_by(|a, b| {
        (a.tier, a.display.to_lowercase()).cmp(&(b.tier, b.display.to_lowercase()))
    });

    Completions {
        start: prefix.word_start,
        items,
    }
}

/// The insertion text for a value: shlex-quoted normally; if the user
/// already opened a quote, complete inside it and close it instead.
fn quoted_insert(value: &str, open_quote: Option<char>) -> Option<String> {
    match open_quote {
        // Single quotes are literal: the value must not contain one.
        Some('\'') => (!value.contains('\'')).then(|| format!("'{value}'")),
        Some(q) => {
            // Double quotes: escape embedded quotes and backslashes.
            let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
            Some(format!("{q}{escaped}{q}"))
        }
        None => quote(value),
    }
}

/// The best inline suggestion: the remainder that, appended at the cursor,
/// extends the current word into the single matching completion.
///
/// Deliberately conservative — a hint is offered only when
/// * the cursor is at the end of the line and a word has been started,
/// * exactly one candidate matches (an ambiguous hint would be a guess;
///   the Tab menu is the place to browse), and
/// * appending is parse-safe: the candidate extends the typed word
///   byte-for-byte, and either needs no quoting or the user already opened
///   a quote (the hint then closes it).
///
/// Candidates that would need re-quoting of already-typed text (e.g. word
/// `unoff` → `Unofficial Skyrim …`) get no inline hint; Tab completes them
/// through the menu, which replaces the whole word with a quoted value.
pub fn hint(snapshot: &Snapshot, opts: Options, line: &str, pos: usize) -> Option<String> {
    if pos != line.len() {
        return None;
    }
    let prefix = tokenize(line, pos);
    if prefix.word.is_empty() {
        return None;
    }
    let completions = complete(snapshot, opts, line, pos);
    let [only] = completions.items.as_slice() else {
        return None;
    };
    let remainder = only.display.strip_prefix(&prefix.word)?;
    if remainder.is_empty() {
        return None;
    }
    match prefix.open_quote {
        Some(q) => Some(format!("{remainder}{q}")),
        None if only.insert == only.display => Some(remainder.to_string()),
        None => None, // would need quoting; menu handles it
    }
}

/// First line of a clap about string, for menu descriptions.
fn one_line_about(cmd: &clap::Command) -> Option<String> {
    cmd.get_about()
        .map(|s| s.to_string().lines().next().unwrap_or_default().to_string())
}

/// `--long` flags of the command at `path`, plus global ones.
fn flags_of(path: &[String]) -> Vec<(String, Option<String>)> {
    let root = Args::command();
    let mut cmd = &root;
    for part in path {
        match cmd.find_subcommand(part) {
            Some(sub) => cmd = sub,
            None => return Vec::new(),
        }
    }
    cmd.get_arguments()
        .filter_map(|a| {
            let long = a.get_long()?;
            let help = a
                .get_help()
                .map(|h| h.to_string().lines().next().unwrap_or_default().to_string());
            Some((format!("--{long}"), help))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Path completion

/// Complete a filesystem path: list the (single) directory the partial path
/// points into — never recursive, so it stays fast in any directory.
fn complete_path(prefix: &LinePrefix, opts: Options) -> Completions {
    let word = &prefix.word;

    // Expand a leading `~/` for lookup but keep what the user typed.
    let (lookup_dir, shown_dir, file_part) = split_path(word);

    let Ok(entries) = std::fs::read_dir(&lookup_dir) else {
        return Completions {
            start: prefix.word_start,
            items: Vec::new(),
        };
    };

    let mut items: Vec<Item> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // Hidden entries only when explicitly asked for.
            if name.starts_with('.') && !file_part.starts_with('.') {
                return None;
            }
            let tier = match_tier(&name, &file_part, false)?;
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let value = format!("{shown_dir}{name}{}", if is_dir { "/" } else { "" });
            let insert = quoted_insert(&value, prefix.open_quote)?;
            Some(Item {
                insert,
                display: value,
                description: opts.descriptions.then(|| {
                    if is_dir {
                        "dir".to_string()
                    } else {
                        "file".to_string()
                    }
                }),
                append_space: !is_dir, // keep typing into directories
                tier,
            })
        })
        .collect();
    items.sort_by_key(|i| (i.tier, i.display.clone()));

    Completions {
        start: prefix.word_start,
        items,
    }
}

/// Split a partial path into (directory to read, prefix to keep showing,
/// file-name part being completed), with `~` expanded for the lookup only.
fn split_path(word: &str) -> (std::path::PathBuf, String, String) {
    let expand = |p: &str| -> std::path::PathBuf {
        if let Some(rest) = p.strip_prefix("~/")
            && let Some(home) = std::env::var_os("HOME")
        {
            return std::path::PathBuf::from(home).join(rest);
        }
        if p.is_empty() {
            return std::path::PathBuf::from(".");
        }
        std::path::PathBuf::from(p)
    };
    match word.rsplit_once('/') {
        Some((dir, file)) => (
            expand(&format!("{dir}/")),
            format!("{dir}/"),
            file.to_string(),
        ),
        None => (expand(""), String::new(), word.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::data::{DownloadEntry, ModEntry};
    use lmm_nexus::queue::Status;

    const OPTS: Options = Options {
        fuzzy: true,
        descriptions: true,
    };

    fn snap() -> Snapshot {
        Snapshot {
            installs: vec!["skyrimse".into(), "testgame".into(), "1".into()],
            mods: vec![
                ModEntry {
                    name: "SkyUI - Modern Interface".into(),
                    enabled: true,
                },
                ModEntry {
                    name: "Unofficial Skyrim Special Edition Patch".into(),
                    enabled: false,
                },
                ModEntry {
                    name: "static-mesh-improvements".into(),
                    enabled: true,
                },
                ModEntry {
                    name: "Ünïcode Mød".into(),
                    enabled: false,
                },
            ],
            profiles: vec!["default".into(), "vanilla-plus".into()],
            downloads: vec![
                DownloadEntry {
                    id: 7,
                    status: Status::Pending,
                    label: "'SkyUI' (SkyUI.7z)".into(),
                },
                DownloadEntry {
                    id: 8,
                    status: Status::Completed,
                    label: "'USSEP' (ussep.7z)".into(),
                },
                DownloadEntry {
                    id: 9,
                    status: Status::Failed,
                    label: "'Foo' (foo.zip)".into(),
                },
            ],
            tools: vec![
                crate::shell::data::ToolEntry {
                    id: "skse".into(),
                    name: "SKSE64".into(),
                },
                crate::shell::data::ToolEntry {
                    id: "loot".into(),
                    name: "LOOT".into(),
                },
            ],
            last_error: None,
        }
    }

    fn values(c: &Completions) -> Vec<&str> {
        c.items.iter().map(|i| i.display.as_str()).collect()
    }

    // -- tokenizer --

    #[test]
    fn tokenize_plain_words() {
        let p = tokenize("disable foo bar", 15);
        assert_eq!(p.tokens, vec!["disable", "foo"]);
        assert_eq!(p.word, "bar");
        assert_eq!(p.word_start, 12);
        assert_eq!(p.open_quote, None);
    }

    #[test]
    fn tokenize_open_double_quote() {
        let p = tokenize(r#"disable "Unoff"#, 14);
        assert_eq!(p.tokens, vec!["disable"]);
        assert_eq!(p.word, "Unoff");
        assert_eq!(p.word_start, 8, "span starts at the opening quote");
        assert_eq!(p.open_quote, Some('"'));
    }

    #[test]
    fn tokenize_closed_quotes_and_next_word() {
        let p = tokenize(r#"enable "Some Mod" ot"#, 20);
        assert_eq!(p.tokens, vec!["enable", "Some Mod"]);
        assert_eq!(p.word, "ot");
    }

    #[test]
    fn tokenize_cursor_mid_line_ignores_tail() {
        //          0123456789
        let line = "disable skyri --json";
        let p = tokenize(line, 13); // cursor right after "skyri"
        assert_eq!(p.tokens, vec!["disable"]);
        assert_eq!(p.word, "skyri");
    }

    #[test]
    fn tokenize_unicode_word() {
        let line = "enable Ünïc";
        let p = tokenize(line, line.len());
        assert_eq!(p.word, "Ünïc");
        assert_eq!(p.word_start, 7);
    }

    #[test]
    fn tokenize_empty_current_word() {
        let p = tokenize("enable ", 7);
        assert_eq!(p.tokens, vec!["enable"]);
        assert_eq!(p.word, "");
        assert_eq!(p.word_start, 7);
    }

    // -- matching --

    #[test]
    fn tiers_are_ordered() {
        assert_eq!(match_tier("SkyUI", "Sky", true), Some(Tier::ExactPrefix));
        assert_eq!(match_tier("SkyUI", "sky", true), Some(Tier::CiPrefix));
        assert_eq!(
            match_tier("Unofficial Patch", "unofficial-p", true),
            Some(Tier::NormalizedPrefix)
        );
        assert_eq!(
            match_tier("SkyUI - Modern Interface", "inter", true),
            Some(Tier::WordPrefix)
        );
        assert_eq!(match_tier("SkyUI", "sui", true), Some(Tier::Fuzzy));
        assert_eq!(match_tier("SkyUI", "sui", false), None, "fuzzy off");
        assert_eq!(match_tier("SkyUI", "zzz", true), None);
    }

    // -- command / subcommand / flags --

    #[test]
    fn completes_command_names() {
        let c = complete(&snap(), OPTS, "dis", 3);
        assert_eq!(values(&c), vec!["disable"]);
        assert_eq!(c.items[0].insert, "disable");
    }

    #[test]
    fn completes_subcommands_with_descriptions() {
        let c = complete(&snap(), OPTS, "profile sw", 10);
        assert_eq!(values(&c), vec!["switch"]);
        assert!(
            c.items[0]
                .description
                .as_deref()
                .unwrap()
                .contains("Switch")
        );
    }

    #[test]
    fn completes_flags() {
        let c = complete(&snap(), OPTS, "deploy --dr", 11);
        assert_eq!(values(&c), vec!["--dry-run"]);
    }

    // -- mods --

    #[test]
    fn enable_offers_only_disabled_mods() {
        let c = complete(&snap(), OPTS, "enable ", 7);
        // Both match at the same tier; ties order by lowercased name, and
        // "unofficial…" byte-compares below "ünïcode…" (U+00FC).
        assert_eq!(
            values(&c),
            vec!["Unofficial Skyrim Special Edition Patch", "Ünïcode Mød"]
        );
        assert_eq!(c.items[1].description.as_deref(), Some("disabled"));
    }

    #[test]
    fn disable_offers_only_enabled_mods() {
        let c = complete(&snap(), OPTS, "disable sky", 11);
        assert_eq!(values(&c), vec!["SkyUI - Modern Interface"]);
        // Insertion is quoted because the name contains spaces, and quoting
        // must round-trip through the parser as ONE argument.
        let insert = &c.items[0].insert;
        let parsed = shlex::split(&format!("disable {insert}")).unwrap();
        assert_eq!(parsed, vec!["disable", "SkyUI - Modern Interface"]);
    }

    #[test]
    fn uninstall_offers_all_mods() {
        let c = complete(&snap(), OPTS, "uninstall ", 10);
        assert_eq!(c.items.len(), 4);
    }

    #[test]
    fn case_insensitive_prefix_beats_word_match() {
        let c = complete(&snap(), OPTS, "disable s", 9);
        // "SkyUI…" (ci-prefix) before "static-mesh…" (exact prefix)? No:
        // "static-mesh…" starts with 's' exactly → ExactPrefix first.
        assert_eq!(
            values(&c),
            vec!["static-mesh-improvements", "SkyUI - Modern Interface"]
        );
    }

    #[test]
    fn unicode_names_complete() {
        let c = complete(&snap(), OPTS, "enable Ünï", 12);
        assert_eq!(values(&c), vec!["Ünïcode Mød"]);
    }

    #[test]
    fn open_quote_completion_closes_the_quote() {
        let line = r#"enable "Unoff"#;
        let c = complete(&snap(), OPTS, line, line.len());
        assert_eq!(values(&c), vec!["Unofficial Skyrim Special Edition Patch"]);
        assert_eq!(c.start, 7, "replaces from the opening quote");
        let parsed = shlex::split(&format!("enable {}", c.items[0].insert)).unwrap();
        assert_eq!(parsed[1], "Unofficial Skyrim Special Edition Patch");
    }

    // -- games / profiles / downloads --

    #[test]
    fn use_completes_install_selectors() {
        let c = complete(&snap(), OPTS, "use test", 8);
        assert_eq!(values(&c), vec!["testgame"]);
        let c = complete(&snap(), OPTS, "game use sky", 12);
        assert_eq!(values(&c), vec!["skyrimse"]);
    }

    #[test]
    fn profile_switch_completes_profiles() {
        let c = complete(&snap(), OPTS, "profile switch van", 18);
        assert_eq!(values(&c), vec!["vanilla-plus"]);
        // `profile create` takes a new name: nothing to suggest.
        let c = complete(&snap(), OPTS, "profile create ", 15);
        assert!(c.items.is_empty());
    }

    #[test]
    fn downloads_ids_filtered_by_subcommand() {
        let c = complete(&snap(), OPTS, "downloads install ", 18);
        assert_eq!(values(&c), vec!["8"], "only completed rows");
        let c = complete(&snap(), OPTS, "downloads cancel ", 17);
        assert_eq!(values(&c), vec!["7"], "only pending/active rows");
        let c = complete(&snap(), OPTS, "downloads retry ", 16);
        assert_eq!(values(&c), vec!["9"], "only failed rows");
        assert!(
            c.items[0]
                .description
                .as_deref()
                .unwrap()
                .contains("failed")
        );
    }

    // -- middle of line, flag values --

    #[test]
    fn completion_works_mid_line() {
        let line = "disable skyu --json";
        let c = complete(&snap(), OPTS, line, 12); // cursor after "skyu"
        assert_eq!(values(&c), vec!["SkyUI - Modern Interface"]);
        assert_eq!(c.start, 8);
    }

    #[test]
    fn flag_value_positions_are_not_completed() {
        // `install --name <cursor>`: --name takes a value (free text).
        let line = "install --name s";
        let c = complete(&snap(), OPTS, line, line.len());
        assert!(c.items.is_empty());
    }

    #[test]
    fn positional_index_skips_flags_and_their_values() {
        // After `--name x`, the next token is still positional 0 (the path
        // for install); for order, arg 1 (the position) is None.
        let line = "order static position-typo-check";
        let c = complete(&snap(), OPTS, line, 12); // completing arg 0
        assert_eq!(values(&c), vec!["static-mesh-improvements"]);
    }

    // -- hints --

    #[test]
    fn hint_completes_unambiguous_command() {
        assert_eq!(hint(&snap(), OPTS, "dis", 3), Some("able".into()));
    }

    #[test]
    fn hint_absent_when_ambiguous() {
        // "de" matches deploy… only? deploy is unique: use "s" instead.
        assert_eq!(hint(&snap(), OPTS, "s", 1), None, "scan/status/… ambiguous");
    }

    #[test]
    fn hint_skips_values_that_would_need_quoting() {
        // Unquoted multiword candidate: appending would split arguments.
        assert_eq!(hint(&snap(), OPTS, "enable Unoff", 12), None);
    }

    #[test]
    fn hint_inside_open_quote_closes_it() {
        let line = r#"enable "Unoff"#;
        let h = hint(&snap(), OPTS, line, line.len()).unwrap();
        assert_eq!(h, r#"icial Skyrim Special Edition Patch""#);
        // Accepting the hint yields a line the parser reads as one argument.
        let full = format!("{line}{h}");
        let parsed = shlex::split(&full).unwrap();
        assert_eq!(parsed[1], "Unofficial Skyrim Special Edition Patch");
    }

    #[test]
    fn hint_only_at_end_of_line() {
        assert_eq!(hint(&snap(), OPTS, "dis --json", 3), None);
    }

    #[test]
    fn hint_requires_case_sensitive_extension() {
        // "skyui…" ci-matches SkyUI but the typed prefix can't be extended.
        assert_eq!(hint(&snap(), OPTS, "disable skyui", 13), None);
    }

    // -- graceful degradation --

    #[test]
    fn empty_snapshot_still_completes_static() {
        let empty = Snapshot::default();
        let c = complete(&empty, OPTS, "dis", 3);
        assert_eq!(values(&c), vec!["disable"]);
        let c = complete(&empty, OPTS, "enable ", 7);
        assert!(c.items.is_empty(), "no mods known, no dynamic candidates");
    }

    // -- quoting --

    #[test]
    fn quote_roundtrips_through_shlex() {
        for name in [
            "plain",
            "with space",
            "it's quoted",
            r#"double "quoted" name"#,
            "Ünïcode Mød",
            "SkyUI - Modern Interface",
        ] {
            let quoted = quote(name).unwrap();
            assert_eq!(shlex::split(&quoted).unwrap(), vec![name], "{name}");
        }
    }
}
