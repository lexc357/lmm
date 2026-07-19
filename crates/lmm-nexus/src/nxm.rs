//! Parsing and validation of `nxm://` links.
//!
//! An nxm link is what the browser hands us when the user clicks
//! "Mod Manager Download" on nexusmods.com:
//!
//! ```text
//! nxm://skyrimspecialedition/mods/12604/files/61527?key=AbC...&expires=1721000000&user_id=1234
//! ```
//!
//! The link arrives over IPC or argv from an untrusted source (any local
//! process can write to our socket, any website can trigger the handler), so
//! parsing is strict: exact scheme, a domain restricted to a safe character
//! set, an exact `/mods/<id>/files/<id>` path, and bounded lengths. Anything
//! else is rejected.
//!
//! The `key`/`expires` pair is a short-lived, per-user download authorization
//! issued by Nexus. It is a credential: [`NxmLink`]'s `Display` deliberately
//! omits it, and nothing in lmm ever logs it.

use std::fmt;

use crate::{Error, Result};

/// Hard cap on the raw link length; real links are ~120 bytes.
const MAX_LINK_LEN: usize = 2048;
/// Nexus game domains are short ASCII slugs ("skyrimspecialedition").
const MAX_DOMAIN_LEN: usize = 64;
/// The download key is a base64-ish token; cap generously.
const MAX_KEY_LEN: usize = 512;

/// A validated Nexus Mods download request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NxmLink {
    /// Nexus game domain, e.g. "skyrimspecialedition". Lowercased.
    pub domain: String,
    pub mod_id: u64,
    pub file_id: u64,
    /// Short-lived download key (credential — never display or log).
    pub key: Option<String>,
    /// Unix time at which `key` stops working.
    pub expires: Option<i64>,
}

impl NxmLink {
    /// Parse and validate an untrusted nxm:// URL string.
    pub fn parse(raw: &str) -> Result<NxmLink> {
        let raw = raw.trim();
        if raw.len() > MAX_LINK_LEN {
            return Err(Error::InvalidLink("link is implausibly long".into()));
        }
        let url = url::Url::parse(raw)
            .map_err(|e| Error::InvalidLink(format!("not a valid URL: {e}")))?;
        if url.scheme() != "nxm" {
            return Err(Error::InvalidLink(format!(
                "scheme is '{}', expected 'nxm'",
                url.scheme()
            )));
        }

        // The "host" of an nxm URL is the Nexus game domain.
        let domain = url
            .host_str()
            .ok_or_else(|| Error::InvalidLink("missing game domain".into()))?
            .to_ascii_lowercase();
        if domain.is_empty()
            || domain.len() > MAX_DOMAIN_LEN
            || !domain.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return Err(Error::InvalidLink(format!(
                "'{domain}' is not a valid Nexus game domain"
            )));
        }

        // Path must be exactly /mods/<id>/files/<id>.
        let segments: Vec<&str> = url
            .path_segments()
            .map(Iterator::collect)
            .unwrap_or_default();
        let (mod_id, file_id) = match segments.as_slice() {
            ["mods", mod_id, "files", file_id] => (parse_id(mod_id)?, parse_id(file_id)?),
            _ => {
                return Err(Error::InvalidLink(
                    "path is not of the form /mods/<id>/files/<id>".into(),
                ));
            }
        };

        // Query: pick out key/expires, ignore unknown parameters (Nexus adds
        // user_id and may add more; none of them matter to us).
        let mut key = None;
        let mut expires = None;
        for (name, value) in url.query_pairs() {
            match name.as_ref() {
                "key" => {
                    if value.is_empty() || value.len() > MAX_KEY_LEN {
                        return Err(Error::InvalidLink("malformed download key".into()));
                    }
                    key = Some(value.into_owned());
                }
                "expires" => {
                    expires = Some(value.parse::<i64>().map_err(|_| {
                        Error::InvalidLink("'expires' is not a unix timestamp".into())
                    })?);
                }
                _ => {}
            }
        }

        Ok(NxmLink {
            domain,
            mod_id,
            file_id,
            key,
            expires,
        })
    }

    /// True if the link carried a download key that is already past expiry.
    pub fn is_expired(&self, now: i64) -> bool {
        matches!((&self.key, self.expires), (Some(_), Some(exp)) if exp <= now)
    }
}

/// Display omits the key/expires credential on purpose — this is the form
/// that may appear in terminal output, errors and logs.
impl fmt::Display for NxmLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "nxm://{}/mods/{}/files/{}",
            self.domain, self.mod_id, self.file_id
        )
    }
}

fn parse_id(s: &str) -> Result<u64> {
    // Leading '+' / '0x' etc. are rejected by from_str_radix-style parsing of
    // u64 only when non-digits appear; be explicit: digits only, bounded.
    if s.is_empty() || s.len() > 12 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::InvalidLink(format!("'{s}' is not a numeric id")));
    }
    s.parse::<u64>()
        .map_err(|_| Error::InvalidLink(format!("'{s}' is not a numeric id")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_link() {
        let l = NxmLink::parse(
            "nxm://skyrimspecialedition/mods/12604/files/61527?key=aBc-12_3&expires=1721000000&user_id=77",
        )
        .unwrap();
        assert_eq!(l.domain, "skyrimspecialedition");
        assert_eq!(l.mod_id, 12604);
        assert_eq!(l.file_id, 61527);
        assert_eq!(l.key.as_deref(), Some("aBc-12_3"));
        assert_eq!(l.expires, Some(1_721_000_000));
    }

    #[test]
    fn parses_link_without_query() {
        let l = NxmLink::parse("nxm://fallout4/mods/1/files/2").unwrap();
        assert_eq!(l.key, None);
        assert_eq!(l.expires, None);
        assert!(!l.is_expired(i64::MAX));
    }

    #[test]
    fn domain_is_lowercased() {
        let l = NxmLink::parse("nxm://SkyrimSpecialEdition/mods/1/files/2").unwrap();
        assert_eq!(l.domain, "skyrimspecialedition");
    }

    #[test]
    fn display_redacts_credentials() {
        let l = NxmLink::parse("nxm://skyrim/mods/3/files/4?key=SECRET&expires=99").unwrap();
        let shown = l.to_string();
        assert_eq!(shown, "nxm://skyrim/mods/3/files/4");
        assert!(!shown.contains("SECRET"));
    }

    #[test]
    fn expiry() {
        let l = NxmLink::parse("nxm://skyrim/mods/3/files/4?key=k&expires=100").unwrap();
        assert!(l.is_expired(100));
        assert!(l.is_expired(101));
        assert!(!l.is_expired(99));
    }

    #[test]
    fn rejects_bad_links() {
        for bad in [
            "https://skyrim/mods/1/files/2",                   // wrong scheme
            "nxm://skyrim/mods/1",                             // short path
            "nxm://skyrim/mods/1/files/2/extra",               // long path
            "nxm://skyrim/mods/x/files/2",                     // non-numeric id
            "nxm://skyrim/mods/-1/files/2",                    // negative id
            "nxm://skyrim/mods/999999999999999999999/files/2", // huge id
            "nxm:///mods/1/files/2",                           // empty domain
            "nxm://bad.domain/mods/1/files/2",                 // dot in domain
            "nxm://sky rim/mods/1/files/2",                    // space in domain
            "nxm://skyrim/mods/1/files/2?key=&expires=1",      // empty key
            "nxm://skyrim/mods/1/files/2?key=k&expires=soon",  // bad expires
            "not a url at all",
        ] {
            assert!(NxmLink::parse(bad).is_err(), "should reject: {bad}");
        }
    }

    #[test]
    fn rejects_oversized_link() {
        let big = format!("nxm://skyrim/mods/1/files/2?key={}", "a".repeat(4096));
        assert!(NxmLink::parse(&big).is_err());
    }
}
