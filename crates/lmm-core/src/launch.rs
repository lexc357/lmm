//! Launching games. The core only decides *how* to launch — the frontend
//! performs the spawn, which is a side effect on the user's session (like
//! printing) and therefore not core business.

use serde::Serialize;

use crate::error::{Error, Result};
use crate::games;
use crate::model::Installation;

#[derive(Debug, Serialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum LaunchMethod {
    /// Open a `steam://rungameid/<appid>` URL; Steam brings up Proton and
    /// the game's own launcher exactly as a desktop launch would.
    SteamUrl { url: String },
}

pub fn method(inst: &Installation) -> Result<LaunchMethod> {
    match games::by_slug(&inst.game_slug).and_then(|g| g.steam_app_id) {
        Some(id) => Ok(LaunchMethod::SteamUrl {
            url: format!("steam://rungameid/{id}"),
        }),
        None => Err(Error::Invalid(format!(
            "'{}' has no Steam app id; start the game manually from {}",
            inst.game_name,
            inst.path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(slug: &str) -> Installation {
        Installation {
            id: 1,
            game_slug: slug.into(),
            game_name: slug.into(),
            path: "/tmp/game".into(),
            source: "manual".into(),
            proton_prefix: None,
            label: None,
            active_profile_id: None,
            created_at: 0,
        }
    }

    #[test]
    fn steam_games_launch_via_url() {
        let LaunchMethod::SteamUrl { url } = method(&inst("skyrimse")).unwrap();
        assert_eq!(url, "steam://rungameid/489830");
    }

    #[test]
    fn generic_games_cannot_be_launched() {
        assert!(method(&inst("generic")).is_err());
    }
}
