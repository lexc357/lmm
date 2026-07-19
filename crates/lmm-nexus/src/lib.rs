//! lmm-nexus: Nexus Mods integration for lmm.
//!
//! Responsibilities, one module each:
//! - [`nxm`]      — parsing and validation of untrusted `nxm://` links
//! - [`api`]      — HTTPS client for the Nexus Mods REST API
//! - [`queue`]    — persistent download queue (rows in lmm's SQLite database)
//! - [`download`] — streaming a single archive to disk, safely
//! - [`ipc`]      — handing links from the browser handler to a running lmm
//! - [`xdg`]      — registering lmm as the system `nxm://` handler
//!
//! Like lmm-core, this crate is interface-agnostic: nothing here prints,
//! prompts, or spawns threads. The CLI/shell decides when to resolve, when to
//! download, and how to report progress.
//!
//! Security stance: nxm links, API responses and downloaded bytes are all
//! untrusted input. Links are strictly validated, file names are sanitized
//! before they touch the filesystem, downloads are size-capped and hashed,
//! and neither API keys, nxm keys nor full download URLs ever appear in
//! errors, logs or display output. See docs/NEXUS.md.

#![cfg_attr(test, allow(clippy::unwrap_used))]

pub mod api;
pub mod download;
pub mod ipc;
pub mod nxm;
pub mod queue;
pub mod xdg;

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The nxm:// link failed validation and must not be processed.
    #[error("invalid nxm link: {0}")]
    InvalidLink(String),

    /// The Nexus API rejected or failed the request. The message is safe to
    /// display: it never contains keys or full URLs.
    #[error("Nexus API: {0}")]
    Api(String),

    /// The short-lived download key from the nxm link has expired.
    #[error("download link expired; click \"Mod Manager Download\" on Nexus Mods again")]
    LinkExpired,

    /// No API key configured; most API calls require one.
    #[error("no Nexus API key configured; run 'nexus apikey' first")]
    NoApiKey,

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("download error: {0}")]
    Download(String),

    /// Another lmm instance already owns the nxm listener socket.
    #[error("another lmm instance is already listening for nxm links")]
    AlreadyListening,

    #[error("{0}")]
    Other(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}
