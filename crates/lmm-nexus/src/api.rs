//! Minimal client for the Nexus Mods REST API (api.nexusmods.com).
//!
//! Only the four endpoints lmm needs:
//! - validate the API key / identify the user
//! - mod metadata (name, version, author)
//! - file metadata (file name, size)
//! - generate download links from an nxm key
//!
//! All requests carry the user's personal API key in the `apikey` header.
//! The key and the signed download URLs the API returns are credentials:
//! they never appear in errors (`Error::Api` messages are built from status
//! codes and response *fields*, not URLs) and must never be logged by
//! callers. Responses are untrusted: bodies are size-capped and
//! deserialized into narrow typed structs; unknown fields are ignored.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::nxm::NxmLink;
use crate::{Error, Result};

const API_BASE: &str = "https://api.nexusmods.com/v1";
/// Metadata responses are small; anything above this is not a real answer.
const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

pub struct NexusClient {
    agent: ureq::Agent,
    api_key: String,
}

/// GET /users/validate.json
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct User {
    pub user_id: i64,
    pub name: String,
    pub is_premium: bool,
}

/// GET /games/{domain}/mods/{id}.json
#[derive(Debug, Clone, Deserialize)]
pub struct ModInfo {
    pub name: Option<String>,
    pub version: Option<String>,
    pub author: Option<String>,
    /// False for hidden/removed mods.
    #[serde(default)]
    pub available: bool,
}

/// GET /games/{domain}/mods/{mod}/files/{file}.json
#[derive(Debug, Clone, Deserialize)]
pub struct FileInfo {
    /// Display name of the file entry.
    pub name: Option<String>,
    /// Actual archive file name, e.g. "SkyUI_5_2_SE-12604-5-2SE.7z".
    pub file_name: Option<String>,
    pub version: Option<String>,
    /// Uncompressed size in bytes (newer API field).
    pub size_in_bytes: Option<u64>,
}

/// One mirror entry from download_link.json. `URI` is a signed, short-lived
/// URL — a credential; do not display or store it beyond the download call.
#[derive(Debug, Clone, Deserialize)]
pub struct DownloadLink {
    #[serde(rename = "URI")]
    pub uri: String,
}

impl NexusClient {
    pub fn new(api_key: String) -> Result<NexusClient> {
        if api_key.trim().is_empty() {
            return Err(Error::NoApiKey);
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .user_agent(format!("lmm/{}", env!("CARGO_PKG_VERSION")))
            .build();
        Ok(NexusClient {
            agent: ureq::Agent::new_with_config(config),
            api_key,
        })
    }

    /// Check the API key and identify the account it belongs to.
    pub fn validate_user(&self) -> Result<User> {
        self.get_json(&format!("{API_BASE}/users/validate.json"), "validate key")
    }

    pub fn mod_info(&self, domain: &str, mod_id: u64) -> Result<ModInfo> {
        self.get_json(
            &format!("{API_BASE}/games/{domain}/mods/{mod_id}.json"),
            "fetch mod info",
        )
    }

    pub fn file_info(&self, domain: &str, mod_id: u64, file_id: u64) -> Result<FileInfo> {
        self.get_json(
            &format!("{API_BASE}/games/{domain}/mods/{mod_id}/files/{file_id}.json"),
            "fetch file info",
        )
    }

    /// Turn a validated nxm link into concrete download URLs.
    ///
    /// Non-premium accounts must supply the link's key/expires pair (that is
    /// what "Mod Manager Download" issues); premium accounts may omit it.
    pub fn download_links(&self, link: &NxmLink) -> Result<Vec<DownloadLink>> {
        let mut url = format!(
            "{API_BASE}/games/{}/mods/{}/files/{}/download_link.json",
            link.domain, link.mod_id, link.file_id
        );
        if let (Some(key), Some(expires)) = (&link.key, link.expires) {
            // The key came from a URL query originally, but re-encode it
            // rather than trusting it to still be URL-safe.
            let key_enc: String = url::form_urlencoded::byte_serialize(key.as_bytes()).collect();
            url.push_str(&format!("?key={key_enc}&expires={expires}"));
        }
        let links: Vec<DownloadLink> = self.get_json(&url, "generate download link")?;
        if links.is_empty() {
            return Err(Error::Api(
                "no download mirrors returned for this file".into(),
            ));
        }
        Ok(links)
    }

    /// Shared GET with the API key header, status mapping and a body cap.
    /// `what` names the operation for error messages (never the URL).
    fn get_json<T: serde::de::DeserializeOwned>(&self, url: &str, what: &str) -> Result<T> {
        let response = self
            .agent
            .get(url)
            .header("apikey", &self.api_key)
            .header("accept", "application/json")
            .call()
            .map_err(|e| map_http_error(e, what))?;
        response
            .into_body()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .read_json::<T>()
            .map_err(|e| Error::Api(format!("could not {what}: bad response: {e}")))
    }
}

/// Map transport/status errors to messages that are safe to show and act on.
fn map_http_error(e: ureq::Error, what: &str) -> Error {
    match e {
        ureq::Error::StatusCode(401) => {
            Error::Api("API key rejected (401); set a new one with 'nexus apikey'".into())
        }
        // Nexus uses 403 both for expired nxm keys and for permission
        // problems (e.g. non-premium account requesting a link without a key).
        ureq::Error::StatusCode(403) => Error::LinkExpired,
        ureq::Error::StatusCode(404) => Error::Api(format!(
            "could not {what}: not found (404) — the mod or file may have been removed"
        )),
        ureq::Error::StatusCode(429) => {
            Error::Api("rate limited by Nexus (429); wait a while and retry".into())
        }
        ureq::Error::StatusCode(code) => Error::Api(format!("could not {what}: HTTP {code}")),
        // Transport errors (DNS, TLS, timeouts) format without the URL in
        // ureq, but keep the message terse regardless.
        other => Error::Api(format!("could not {what}: {other}")),
    }
}

/// Mask an API key for display: show enough to recognize it, nothing more.
pub fn mask_key(key: &str) -> String {
    let tail: String = key
        .chars()
        .rev()
        .take(4)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    if key.len() <= 8 {
        "****".into()
    } else {
        format!("****{tail}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_key_is_rejected() {
        assert!(matches!(
            NexusClient::new("  ".into()),
            Err(Error::NoApiKey)
        ));
    }

    #[test]
    fn mask_key_shows_only_tail() {
        assert_eq!(mask_key("abcd"), "****");
        assert_eq!(mask_key("abcdefghijkl"), "****ijkl");
    }

    #[test]
    fn status_mapping() {
        assert!(matches!(
            map_http_error(ureq::Error::StatusCode(403), "x"),
            Error::LinkExpired
        ));
        let msg = map_http_error(ureq::Error::StatusCode(500), "fetch mod info").to_string();
        assert!(msg.contains("HTTP 500"), "{msg}");
    }
}
