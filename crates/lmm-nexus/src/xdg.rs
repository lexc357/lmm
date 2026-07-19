//! Registering lmm as the desktop's `nxm://` URL handler.
//!
//! Standard XDG plumbing, nothing exotic:
//! 1. write `lmm-nxm.desktop` into `$XDG_DATA_HOME/applications/`, whose
//!    `Exec` line runs `lmm nxm %u` with the clicked URL;
//! 2. tell the MIME system that this desktop file is the default handler for
//!    the `x-scheme-handler/nxm` pseudo-MIME-type (`xdg-mime default ...`);
//! 3. refresh the desktop-file cache (`update-desktop-database`, optional —
//!    not all systems have or need it).
//!
//! After this, a browser resolving an nxm link asks the desktop portal,
//! which spawns `lmm nxm <url>`; that short-lived process forwards the link
//! to a running lmm or spools it (see [`crate::ipc`]).

use std::path::PathBuf;
use std::process::Command;

use crate::{Error, Result};

pub const DESKTOP_ID: &str = "lmm-nxm.desktop";
pub const NXM_MIME: &str = "x-scheme-handler/nxm";

/// What `register` did, for reporting to the user.
#[derive(Debug, serde::Serialize)]
pub struct Registration {
    pub desktop_file: PathBuf,
    /// Whether `xdg-mime default` succeeded (it is what makes clicks work).
    pub mime_default_set: bool,
    /// Human-readable notes about best-effort steps that failed.
    pub notes: Vec<String>,
}

/// Current registration state, for `nexus status`.
#[derive(Debug, serde::Serialize)]
pub struct HandlerStatus {
    pub desktop_file: PathBuf,
    pub desktop_file_exists: bool,
    /// The desktop id the system currently resolves nxm links to, if any.
    pub current_handler: Option<String>,
}

fn applications_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home =
                std::env::var_os("HOME").ok_or_else(|| Error::Other("HOME is not set".into()))?;
            PathBuf::from(home).join(".local").join("share")
        }
    };
    Ok(base.join("applications"))
}

/// Register the currently running lmm binary as the nxm handler.
pub fn register() -> Result<Registration> {
    let exe = std::env::current_exe()
        .map_err(|e| Error::Other(format!("cannot determine lmm's own path: {e}")))?;
    let exe = exe.canonicalize().map_err(|e| Error::io(&exe, e))?;
    let exe_str = exe.to_str().ok_or_else(|| {
        Error::Other("lmm's path is not valid UTF-8; cannot write a desktop file for it".into())
    })?;
    // Desktop-entry Exec quoting handles spaces but its escaping rules for
    // quotes/backslashes are a swamp; refuse the exotic cases outright.
    if exe_str.contains(['"', '\\', '\n', '`', '$']) {
        return Err(Error::Other(format!(
            "lmm's path ({exe_str}) contains characters that cannot be safely \
             quoted in a .desktop Exec line; install lmm at a plainer path"
        )));
    }

    let dir = applications_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
    let desktop_file = dir.join(DESKTOP_ID);
    let content = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Linux Mod Manager (nxm handler)\n\
         Comment=Queue Nexus Mods downloads in lmm\n\
         Exec=\"{exe_str}\" nxm %u\n\
         MimeType={NXM_MIME};\n\
         NoDisplay=true\n\
         Terminal=false\n"
    );
    std::fs::write(&desktop_file, content).map_err(|e| Error::io(&desktop_file, e))?;

    let mut notes = Vec::new();

    // This is the step that actually routes clicks to us.
    let mime_default_set = match Command::new("xdg-mime")
        .args(["default", DESKTOP_ID, NXM_MIME])
        .status()
    {
        Ok(s) if s.success() => true,
        Ok(s) => {
            notes.push(format!(
                "xdg-mime exited with {s}; nxm links may not open lmm"
            ));
            false
        }
        Err(e) => {
            notes.push(format!(
                "xdg-mime not runnable ({e}); install xdg-utils and rerun 'nexus register'"
            ));
            false
        }
    };

    // Cache refresh is best-effort; many systems pick the file up without it.
    if let Err(e) = Command::new("update-desktop-database").arg(&dir).status() {
        notes.push(format!(
            "update-desktop-database not run ({e}); usually harmless"
        ));
    }

    Ok(Registration {
        desktop_file,
        mime_default_set,
        notes,
    })
}

/// Remove the handler registration (the desktop file; the MIME default
/// becomes dangling and the desktop ignores it).
pub fn unregister() -> Result<Option<PathBuf>> {
    let desktop_file = applications_dir()?.join(DESKTOP_ID);
    match std::fs::remove_file(&desktop_file) {
        Ok(()) => Ok(Some(desktop_file)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(Error::io(&desktop_file, e)),
    }
}

/// Report whether lmm is (still) the registered nxm handler.
pub fn status() -> Result<HandlerStatus> {
    let desktop_file = applications_dir()?.join(DESKTOP_ID);
    let current_handler = Command::new("xdg-mime")
        .args(["query", "default", NXM_MIME])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(HandlerStatus {
        desktop_file_exists: desktop_file.exists(),
        desktop_file,
        current_handler,
    })
}
