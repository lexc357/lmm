//! "Check Modding Setup": one composed pass over everything Game Tools
//! knows — tools, configuration, script extender, plugin list, pending
//! deployments — reported as a flat checklist any frontend can render.

use serde::Serialize;

use crate::Context;
use crate::error::Result;
use crate::model::Installation;
use crate::tools::registry::{GameTools, Tier, ToolKind};
use crate::tools::{ToolState, gameconfig, loadorder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// All good.
    Ok,
    /// Works, but worth improving.
    Warn,
    /// Modding will not work correctly until this is fixed.
    Fail,
    /// Could not be checked (e.g. no Proton prefix yet).
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    /// What to do about it, when not Ok.
    pub recommendation: Option<String>,
}

fn check(name: &str, status: CheckStatus, detail: impl Into<String>) -> Check {
    Check {
        name: name.to_string(),
        status,
        detail: detail.into(),
        recommendation: None,
    }
}

fn recommend(mut c: Check, r: impl Into<String>) -> Check {
    c.recommendation = Some(r.into());
    c
}

/// Run every applicable check for the installation.
pub fn run(ctx: &Context, inst: &Installation, game: &'static GameTools) -> Result<Vec<Check>> {
    let mut out = Vec::new();

    // Game directory sanity.
    out.push(if inst.path.is_dir() {
        check(
            "game directory",
            CheckStatus::Ok,
            inst.path.display().to_string(),
        )
    } else {
        recommend(
            check(
                "game directory",
                CheckStatus::Fail,
                format!("{} does not exist", inst.path.display()),
            ),
            "the game moved or was uninstalled; re-register it with 'game add'",
        )
    });

    // Interrupted deployment blocks everything else lmm does.
    if crate::deploy::find_running(&ctx.db, inst.id)?.is_some() {
        out.push(recommend(
            check(
                "deployment state",
                CheckStatus::Fail,
                "an interrupted deployment is pending",
            ),
            "run 'lmm rollback' before anything else",
        ));
    } else {
        out.push(check(
            "deployment state",
            CheckStatus::Ok,
            "no pending deployment",
        ));
    }

    // Tools: essentials fail when missing, recommended ones warn.
    let statuses = crate::tools::status(ctx, inst)?;
    for st in &statuses {
        let name = format!("tool: {}", st.name);
        let c = match (st.state, st.tier) {
            (ToolState::Installed, _) => check(&name, CheckStatus::Ok, "installed"),
            (ToolState::Outdated, _) => recommend(
                check(
                    &name,
                    CheckStatus::Warn,
                    st.detail.clone().unwrap_or_else(|| "outdated".into()),
                ),
                format!(
                    "update it: 'tools install {} <archive>' ({})",
                    st.id, st.url
                ),
            ),
            (ToolState::Attention, _) => recommend(
                check(
                    &name,
                    CheckStatus::Warn,
                    st.detail
                        .clone()
                        .unwrap_or_else(|| "needs attention".into()),
                ),
                format!("check with 'tools verify {}' or reinstall", st.id),
            ),
            (ToolState::Missing, Tier::Essential) => recommend(
                check(&name, CheckStatus::Fail, "not installed"),
                format!(
                    "download it from {} and run 'tools install {} <archive>'",
                    st.url, st.id
                ),
            ),
            (ToolState::Missing, Tier::Recommended) => recommend(
                check(&name, CheckStatus::Warn, "not installed"),
                format!("recommended for most setups: {}", st.url),
            ),
            (ToolState::Missing, Tier::Optional) => {
                check(&name, CheckStatus::Ok, "not installed (optional)")
            }
        };
        out.push(c);
    }

    // Script extender called out explicitly: mods silently misbehave
    // without one, so make it its own headline check.
    if let Some(ext) = game
        .tools
        .iter()
        .find(|t| t.kind == ToolKind::ScriptExtender)
    {
        let found = statuses
            .iter()
            .find(|s| s.id == ext.id)
            .is_some_and(|s| matches!(s.state, ToolState::Installed | ToolState::Outdated));
        out.push(if found {
            check(
                "script extender",
                CheckStatus::Ok,
                format!("{} detected", ext.name),
            )
        } else {
            recommend(
                check(
                    "script extender",
                    CheckStatus::Fail,
                    format!("{} not detected", ext.name),
                ),
                format!("most mods need it; see {}", ext.url),
            )
        });
    }

    // Configuration tweaks.
    if !game.tweaks.is_empty() {
        match gameconfig::status(inst, game) {
            Ok(tweaks) => {
                let missing: Vec<&gameconfig::TweakStatus> = tweaks
                    .iter()
                    .filter(|t| t.state != gameconfig::TweakState::Applied)
                    .collect();
                out.push(if missing.is_empty() {
                    check(
                        "game configuration",
                        CheckStatus::Ok,
                        format!("all {} modding setting(s) applied", tweaks.len()),
                    )
                } else {
                    recommend(
                        check(
                            "game configuration",
                            CheckStatus::Fail,
                            format!(
                                "{} setting(s) not applied: {}",
                                missing.len(),
                                missing
                                    .iter()
                                    .map(|t| t.id.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            ),
                        ),
                        "run 'tools config apply'",
                    )
                });
            }
            Err(e) => out.push(recommend(
                check("game configuration", CheckStatus::Skip, e.to_string()),
                "run the game once through Steam, then re-check",
            )),
        }
    }

    // Plugin list.
    if game.plugins.is_some() {
        match loadorder::analyze(inst, game) {
            Ok(a) => {
                if !a.path.exists() {
                    out.push(recommend(
                        check(
                            "plugin list",
                            CheckStatus::Skip,
                            "plugins.txt does not exist yet",
                        ),
                        "run the game once through Steam so it creates its plugin list",
                    ));
                } else if a.issues.is_empty() {
                    out.push(check(
                        "plugin list",
                        CheckStatus::Ok,
                        format!("{} plugin(s), no issues", a.plugins.len()),
                    ));
                } else {
                    out.push(recommend(
                        check(
                            "plugin list",
                            CheckStatus::Warn,
                            format!(
                                "{} issue(s) in {} plugin(s)",
                                a.issues.len(),
                                a.plugins.len()
                            ),
                        ),
                        "see 'tools loadorder' and fix with 'tools loadorder sort'",
                    ));
                }
            }
            Err(e) => out.push(recommend(
                check("plugin list", CheckStatus::Skip, e.to_string()),
                "run the game once through Steam, then re-check",
            )),
        }
    }

    Ok(out)
}
