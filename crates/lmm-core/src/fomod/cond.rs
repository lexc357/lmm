//! FOMOD dependency evaluation: pure tri-state logic over an abstract
//! environment.
//!
//! Three concerns are kept apart on purpose:
//! * the *condition tree* comes from parsing (`model::Composite`),
//! * *facts about the machine* come from an [`Environment`] implementation
//!   (the game adapter — see `fomod::env`),
//! * this module only combines the two.
//!
//! Evaluation is three-valued: some questions ("is the game version at
//! least 1.5.97?") cannot be answered reliably on Linux/Proton. Rather than
//! guessing, those evaluate to [`Eval::Unknown`] with a reason, and the
//! caller decides what an unknown means in its context (ask the user, warn,
//! or refuse). Environment errors also degrade to Unknown: a broken stat
//! call must not crash an installer, but it also must not silently count as
//! "no".

use std::collections::BTreeMap;

use crate::error::Result;

use super::model::{Composite, Condition, FileState, Op};

/// Three-valued result of evaluating a condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Eval {
    True,
    False,
    /// Cannot be determined; the string says why (shown to the user).
    Unknown(String),
}

impl Eval {
    pub fn from_bool(b: bool) -> Eval {
        if b { Eval::True } else { Eval::False }
    }
}

/// Facts the evaluator may ask about the installation. Implementations live
/// with the game adapter; tests use maps.
pub trait Environment {
    /// State of a file the game would see (path relative to the mod root,
    /// e.g. "SkyUI.esp" or "meshes/actor.nif"): active, present-but-
    /// inactive, or missing.
    fn file_state(&self, file: &str) -> Result<FileState>;
    /// Installed game version, when it can be determined.
    fn game_version(&self) -> Result<Option<Version>>;
    /// Installed script-extender version, when it can be determined.
    fn script_extender_version(&self) -> Result<Option<Version>>;
}

/// Flags set by selected options so far. Missing flag == empty value, the
/// behavior mod authors rely on ("flagDependency value=''" for "not set").
pub type Flags = BTreeMap<String, String>;

pub fn flag_value<'a>(flags: &'a Flags, name: &str) -> &'a str {
    flags.get(name).map(String::as_str).unwrap_or("")
}

/// Evaluate a composite: And = all parts, Or = any part. Unknown behaves
/// like NaN: it only decides the outcome when the known parts don't.
pub fn eval_composite(c: &Composite, flags: &Flags, env: &dyn Environment) -> Eval {
    let mut unknown: Option<String> = None;
    for part in &c.parts {
        match (c.op, eval_condition(part, flags, env)) {
            (Op::And, Eval::False) => return Eval::False,
            (Op::Or, Eval::True) => return Eval::True,
            (_, Eval::Unknown(why)) => unknown = unknown.or(Some(why)),
            _ => {}
        }
    }
    match (unknown, c.op) {
        (Some(why), _) => Eval::Unknown(why),
        // An empty <dependencies> is trivially satisfied for And; for Or
        // there is nothing that could be true.
        (None, Op::And) => Eval::True,
        (None, Op::Or) => Eval::from_bool(c.parts.is_empty()),
    }
}

pub fn eval_condition(cond: &Condition, flags: &Flags, env: &dyn Environment) -> Eval {
    match cond {
        Condition::Flag { flag, value } => {
            Eval::from_bool(flag_value(flags, flag).trim() == value.trim())
        }
        Condition::File { file, state } => match env.file_state(file) {
            Ok(actual) => Eval::from_bool(actual == *state),
            Err(e) => Eval::Unknown(format!("could not check file '{file}': {e}")),
        },
        Condition::Game { version } => version_at_least(
            env.game_version(),
            version,
            "game version",
            "game versions are not detectable under Linux/Proton",
        ),
        Condition::ScriptExtender { version } => version_at_least(
            env.script_extender_version(),
            version,
            "script extender version",
            "script extender versions are not detectable under Linux/Proton",
        ),
        // lmm is not FOMM; like other managers we satisfy the check rather
        // than block mods that declare a legacy manager version.
        Condition::ModManager { .. } => Eval::True,
        Condition::Nested(inner) => eval_composite(inner, flags, env),
    }
}

fn version_at_least(
    actual: Result<Option<Version>>,
    required: &str,
    what: &str,
    unknown_reason: &str,
) -> Eval {
    let Some(required) = Version::parse(required) else {
        return Eval::Unknown(format!("unparseable required {what} '{required}'"));
    };
    match actual {
        Ok(Some(v)) => Eval::from_bool(v >= required),
        Ok(None) => Eval::Unknown(unknown_reason.to_string()),
        Err(e) => Eval::Unknown(format!("could not determine {what}: {e}")),
    }
}

/// Render a condition tree as one line of human text, for "why is this
/// hidden/locked" explanations and `fomod validate` output.
pub fn describe_composite(c: &Composite) -> String {
    let sep = match c.op {
        Op::And => " AND ",
        Op::Or => " OR ",
    };
    if c.parts.is_empty() {
        return "(no conditions)".into();
    }
    c.parts
        .iter()
        .map(describe_condition)
        .collect::<Vec<_>>()
        .join(sep)
}

pub fn describe_condition(cond: &Condition) -> String {
    match cond {
        Condition::Flag { flag, value } if value.is_empty() => format!("flag '{flag}' is unset"),
        Condition::Flag { flag, value } => format!("flag '{flag}' = '{value}'"),
        Condition::File { file, state } => {
            let s = match state {
                FileState::Active => "active",
                FileState::Inactive => "inactive",
                FileState::Missing => "missing",
            };
            format!("file '{file}' is {s}")
        }
        Condition::Game { version } => format!("game version >= {version}"),
        Condition::ScriptExtender { version } => format!("script extender >= {version}"),
        Condition::ModManager { version } => format!("mod manager >= {version} (always satisfied)"),
        Condition::Nested(inner) => format!("({})", describe_composite(inner)),
    }
}

/// Per-leaf evaluation report: each line pairs a leaf's description with
/// its current status. Used by the UI to explain a locked option and by
/// `fomod validate` to list unsupported conditions.
pub fn explain_composite(c: &Composite, flags: &Flags, env: &dyn Environment) -> Vec<String> {
    let mut lines = Vec::new();
    for part in &c.parts {
        if let Condition::Nested(inner) = part {
            lines.extend(explain_composite(inner, flags, env));
            continue;
        }
        let status = match eval_condition(part, flags, env) {
            Eval::True => "satisfied".to_string(),
            Eval::False => "not satisfied".to_string(),
            Eval::Unknown(why) => format!("unknown: {why}"),
        };
        lines.push(format!("{} — {}", describe_condition(part), status));
    }
    lines
}

/// A dotted version, compared numerically segment by segment (missing
/// segments count as zero, so 1.5 == 1.5.0). Non-numeric suffixes like
/// "1.5.97SE" drop the trailing junk; a fully non-numeric string fails.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(Vec<u64>);

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let mut parts = Vec::new();
        for seg in s.trim().split(['.', ',', '-']) {
            let digits: String = seg.chars().take_while(|c| c.is_ascii_digit()).collect();
            match digits.parse::<u64>() {
                Ok(n) => parts.push(n),
                Err(_) => break, // "1.5.beta" compares as 1.5
            }
        }
        while parts.last() == Some(&0) {
            parts.pop(); // normalize so 1.5.0 == 1.5
        }
        if parts.is_empty() && !s.trim().starts_with('0') {
            return None;
        }
        Some(Version(parts))
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return f.write_str("0");
        }
        let s: Vec<String> = self.0.iter().map(u64::to_string).collect();
        f.write_str(&s.join("."))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    /// Test environment: fixed file states and versions.
    #[derive(Default)]
    pub struct FakeEnv {
        pub files: BTreeMap<String, FileState>,
        pub game: Option<Version>,
        pub skse: Option<Version>,
        pub fail_files: bool,
    }

    impl Environment for FakeEnv {
        fn file_state(&self, file: &str) -> Result<FileState> {
            if self.fail_files {
                return Err(Error::Invalid("boom".into()));
            }
            Ok(self
                .files
                .get(&file.to_lowercase())
                .copied()
                .unwrap_or(FileState::Missing))
        }
        fn game_version(&self) -> Result<Option<Version>> {
            Ok(self.game.clone())
        }
        fn script_extender_version(&self) -> Result<Option<Version>> {
            Ok(self.skse.clone())
        }
    }

    fn flag(name: &str, value: &str) -> Condition {
        Condition::Flag {
            flag: name.into(),
            value: value.into(),
        }
    }

    fn composite(op: Op, parts: Vec<Condition>) -> Composite {
        Composite { op, parts }
    }

    #[test]
    fn version_parsing_and_ordering() {
        let v = |s| Version::parse(s).unwrap();
        assert!(v("1.5.97") > v("1.5.3"));
        assert!(v("1.10") > v("1.9"));
        assert_eq!(v("1.5.0"), v("1.5"));
        assert!(v("2.0.20") >= v("2.0.20"));
        assert_eq!(v("1.5.97SE"), v("1.5.97"));
        assert_eq!(v("1.5.beta"), v("1.5"));
        assert!(Version::parse("nope").is_none());
        assert_eq!(v("0.0.0").to_string(), "0");
    }

    #[test]
    fn flags_compare_with_unset_as_empty() {
        let mut flags = Flags::new();
        let env = FakeEnv::default();
        // Unset flag equals empty value.
        assert_eq!(eval_condition(&flag("f", ""), &flags, &env), Eval::True);
        assert_eq!(eval_condition(&flag("f", "on"), &flags, &env), Eval::False);
        flags.insert("f".into(), "on".into());
        assert_eq!(eval_condition(&flag("f", "on"), &flags, &env), Eval::True);
        assert_eq!(eval_condition(&flag("f", ""), &flags, &env), Eval::False);
    }

    #[test]
    fn file_states_match_exactly() {
        let mut env = FakeEnv::default();
        env.files.insert("skyui.esp".into(), FileState::Active);
        env.files.insert("old.esp".into(), FileState::Inactive);
        let flags = Flags::new();
        let file = |f: &str, s| Condition::File {
            file: f.into(),
            state: s,
        };
        assert_eq!(
            eval_condition(&file("SkyUI.esp", FileState::Active), &flags, &env),
            Eval::True
        );
        assert_eq!(
            eval_condition(&file("old.esp", FileState::Active), &flags, &env),
            Eval::False
        );
        assert_eq!(
            eval_condition(&file("old.esp", FileState::Inactive), &flags, &env),
            Eval::True
        );
        // Missing is FOMOD's negation.
        assert_eq!(
            eval_condition(&file("gone.esp", FileState::Missing), &flags, &env),
            Eval::True
        );
    }

    #[test]
    fn and_or_combinators() {
        let flags: Flags = [("a".to_string(), "1".to_string())].into();
        let env = FakeEnv::default();
        let t = flag("a", "1");
        let f = flag("a", "2");

        let and_tf = composite(Op::And, vec![t.clone(), f.clone()]);
        assert_eq!(eval_composite(&and_tf, &flags, &env), Eval::False);
        let and_tt = composite(Op::And, vec![t.clone(), t.clone()]);
        assert_eq!(eval_composite(&and_tt, &flags, &env), Eval::True);
        let or_tf = composite(Op::Or, vec![f.clone(), t.clone()]);
        assert_eq!(eval_composite(&or_tf, &flags, &env), Eval::True);
        let or_ff = composite(Op::Or, vec![f.clone(), f.clone()]);
        assert_eq!(eval_composite(&or_ff, &flags, &env), Eval::False);
        // Empty composites: And = true, Or = true (vacuous, matches refs).
        assert_eq!(
            eval_composite(&composite(Op::And, vec![]), &flags, &env),
            Eval::True
        );
        assert_eq!(
            eval_composite(&composite(Op::Or, vec![]), &flags, &env),
            Eval::True
        );
    }

    #[test]
    fn nested_composites() {
        let flags: Flags = [("a".to_string(), "1".to_string())].into();
        let env = FakeEnv::default();
        // (a=2 OR (a=1 AND a=1)) => True
        let inner = composite(Op::And, vec![flag("a", "1"), flag("a", "1")]);
        let outer = composite(Op::Or, vec![flag("a", "2"), Condition::Nested(inner)]);
        assert_eq!(eval_composite(&outer, &flags, &env), Eval::True);
    }

    #[test]
    fn unknown_only_decides_when_it_must() {
        let flags: Flags = [("a".to_string(), "1".to_string())].into();
        let env = FakeEnv::default(); // no game version -> Unknown
        let unknown = Condition::Game {
            version: "1.5".into(),
        };
        // And with a False short-circuits regardless of the Unknown.
        let c = composite(Op::And, vec![flag("a", "2"), unknown.clone()]);
        assert_eq!(eval_composite(&c, &flags, &env), Eval::False);
        // Or with a True short-circuits too.
        let c = composite(Op::Or, vec![flag("a", "1"), unknown.clone()]);
        assert_eq!(eval_composite(&c, &flags, &env), Eval::True);
        // Otherwise Unknown surfaces, with the reason.
        let c = composite(Op::And, vec![flag("a", "1"), unknown.clone()]);
        match eval_composite(&c, &flags, &env) {
            Eval::Unknown(why) => assert!(why.contains("Linux/Proton"), "{why}"),
            other => panic!("expected unknown, got {other:?}"),
        }
    }

    #[test]
    fn known_versions_evaluate() {
        let flags = Flags::new();
        let env = FakeEnv {
            game: Version::parse("1.6.640"),
            skse: Version::parse("2.2.3"),
            ..FakeEnv::default()
        };
        let game_ok = Condition::Game {
            version: "1.5.97".into(),
        };
        let game_newer = Condition::Game {
            version: "1.7".into(),
        };
        let skse_ok = Condition::ScriptExtender {
            version: "2.0".into(),
        };
        assert_eq!(eval_condition(&game_ok, &flags, &env), Eval::True);
        assert_eq!(eval_condition(&game_newer, &flags, &env), Eval::False);
        assert_eq!(eval_condition(&skse_ok, &flags, &env), Eval::True);
    }

    #[test]
    fn environment_errors_become_unknown() {
        let flags = Flags::new();
        let env = FakeEnv {
            fail_files: true,
            ..FakeEnv::default()
        };
        let c = Condition::File {
            file: "a.esp".into(),
            state: FileState::Active,
        };
        assert!(matches!(eval_condition(&c, &flags, &env), Eval::Unknown(_)));
    }

    #[test]
    fn fomm_dependency_is_always_satisfied() {
        assert_eq!(
            eval_condition(
                &Condition::ModManager {
                    version: "0.13".into()
                },
                &Flags::new(),
                &FakeEnv::default()
            ),
            Eval::True
        );
    }

    #[test]
    fn describe_and_explain() {
        let c = composite(
            Op::Or,
            vec![
                flag("res", "high"),
                Condition::Nested(composite(
                    Op::And,
                    vec![Condition::Game {
                        version: "1.5".into(),
                    }],
                )),
            ],
        );
        let text = describe_composite(&c);
        assert_eq!(text, "flag 'res' = 'high' OR (game version >= 1.5)");
        let flags = Flags::new();
        let env = FakeEnv::default();
        let lines = explain_composite(&c, &flags, &env);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("not satisfied"));
        assert!(lines[1].contains("unknown"));
    }
}
