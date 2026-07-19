//! Streaming a single archive from a (signed, short-lived) URL to disk.
//!
//! Safety properties:
//! - the destination file name is sanitized here, not trusted from the API;
//! - bytes stream through a hard size cap — a lying `Content-Length` or an
//!   endless body cannot fill the disk;
//! - data goes to a temp file in the destination directory and is fsync'd
//!   and renamed into place only on success, so a crash never leaves a
//!   half-written archive under a real name;
//! - the SHA-256 of everything written is computed on the fly and returned,
//!   and is later checked by the archive import pipeline.
//!
//! The URL is a credential (it embeds a signed token); it is accepted as a
//! parameter and never stored, displayed, or included in errors.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// Report a completed transfer.
#[derive(Debug)]
pub struct Downloaded {
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

/// Progress notifications every ~this many bytes.
const PROGRESS_EVERY: u64 = 1024 * 1024;
const CHUNK: usize = 64 * 1024;

/// Download `uri` into `dest_dir` as (a sanitized form of) `preferred_name`.
///
/// `progress(bytes_done)` is called about once per MiB; returning `false`
/// cancels the transfer (that is how `downloads cancel` reaches a worker).
/// `max_bytes` is a hard cap on the body size regardless of headers.
pub fn fetch(
    uri: &str,
    dest_dir: &Path,
    preferred_name: &str,
    max_bytes: u64,
    mut progress: impl FnMut(u64) -> bool,
) -> Result<Downloaded> {
    // Only https from the Nexus CDN makes sense here; refuse anything else
    // (the API response is untrusted input too).
    if !uri.starts_with("https://") {
        return Err(Error::Download("mirror returned a non-https URL".into()));
    }

    // Connect/response timeouts, but no whole-transfer timeout: large
    // archives on slow links legitimately take a long time.
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .user_agent(format!("lmm/{}", env!("CARGO_PKG_VERSION")))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let response = agent.get(uri).call().map_err(|e| match e {
        // Signed CDN URLs return 403 once the token lapses.
        ureq::Error::StatusCode(403) => Error::LinkExpired,
        ureq::Error::StatusCode(code) => Error::Download(format!("mirror returned HTTP {code}")),
        other => Error::Download(format!("transfer failed: {other}")),
    })?;
    let mut reader = response.into_body().into_reader();

    // Temp file in the destination dir so the final move is an atomic rename.
    let mut tmp = tempfile::Builder::new()
        .prefix(".lmm-download-")
        .tempfile_in(dest_dir)
        .map_err(|e| Error::io(dest_dir, e))?;

    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut last_reported: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| Error::Download(format!("transfer failed after {total} bytes: {e}")))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max_bytes {
            return Err(Error::Download(format!(
                "download exceeds the size limit ({max_bytes} bytes); \
                 raise [limits] in the config if this is intentional"
            )));
        }
        hasher.update(&buf[..n]);
        tmp.write_all(&buf[..n])
            .map_err(|e| Error::io(tmp.path(), e))?;
        if total - last_reported >= PROGRESS_EVERY {
            last_reported = total;
            if !progress(total) {
                return Err(Error::Download("cancelled".into()));
            }
        }
    }
    if total == 0 {
        return Err(Error::Download("mirror returned an empty file".into()));
    }
    progress(total);

    // Durability before visibility: fsync the data, then rename into place.
    tmp.as_file()
        .sync_all()
        .map_err(|e| Error::io(tmp.path(), e))?;
    let final_path = unique_path(dest_dir, &sanitize_file_name(preferred_name));
    tmp.persist_noclobber(&final_path)
        .map_err(|e| Error::io(&final_path, e.error))?;

    Ok(Downloaded {
        path: final_path,
        sha256: lmm_core::hash::hex(&hasher.finalize()),
        size: total,
    })
}

/// Reduce an untrusted file name to something safe to create in the
/// downloads directory: no path separators, no leading dots, printable
/// ASCII-ish subset, bounded length, never empty.
pub fn sanitize_file_name(name: &str) -> String {
    // Anything that looks like a path keeps only its final component.
    let name = name.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut out: String = name
        .chars()
        .map(|c| match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' | '+' | '(' | ')' | ' ' => c,
            _ => '_',
        })
        .collect();
    // No hidden files, no "..", no leading/trailing whitespace confusion.
    while out.starts_with(['.', ' ']) {
        out.remove(0);
    }
    out.truncate(150);
    let out = out.trim().to_string();
    if out.is_empty() {
        "download.bin".to_string()
    } else {
        out
    }
}

/// First non-existing variant of `name` in `dir`: "x.7z", "x (2).7z", ...
fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.to_string(), String::new()),
    };
    for i in 2.. {
        let candidate = dir.join(format!("{stem} ({i}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("some suffix is always free")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_dangerous_names() {
        assert_eq!(
            sanitize_file_name("SkyUI_5_2_SE-12604.7z"),
            "SkyUI_5_2_SE-12604.7z"
        );
        assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_file_name(".hidden"), "hidden");
        assert_eq!(sanitize_file_name("a/b\\c"), "c");
        assert_eq!(sanitize_file_name(""), "download.bin");
        assert_eq!(sanitize_file_name("evil/.."), "download.bin");
        assert_eq!(sanitize_file_name("..."), "download.bin");
        assert_eq!(sanitize_file_name("naïve\u{202e}name.7z"), "na_ve_name.7z");
        assert!(sanitize_file_name(&"x".repeat(400)).len() <= 150);
    }

    #[test]
    fn unique_path_appends_counter() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(unique_path(dir.path(), "a.7z"), dir.path().join("a.7z"));
        std::fs::write(dir.path().join("a.7z"), b"x").unwrap();
        assert_eq!(unique_path(dir.path(), "a.7z"), dir.path().join("a (2).7z"));
        std::fs::write(dir.path().join("a (2).7z"), b"x").unwrap();
        assert_eq!(unique_path(dir.path(), "a.7z"), dir.path().join("a (3).7z"));
    }

    #[test]
    fn rejects_plain_http() {
        let dir = tempfile::tempdir().unwrap();
        let err = fetch("http://example.com/x", dir.path(), "x", 1024, |_| true).unwrap_err();
        assert!(err.to_string().contains("non-https"), "{err}");
    }
}
