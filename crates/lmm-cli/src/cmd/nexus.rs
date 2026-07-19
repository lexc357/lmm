//! `nexus ...` — Nexus Mods account and nxm:// handler setup.

use std::io::IsTerminal;

use anyhow::{Result, bail};
use lmm_core::Context;
use lmm_nexus::api::{NexusClient, mask_key};
use serde::Serialize;

use crate::args::NexusCmd;
use crate::output::Out;

/// Settings-table key holding the user's Nexus API key. The database file
/// lives in the user's data dir with their regular file permissions; the key
/// is never printed (only masked) and never sent anywhere but api.nexusmods.com.
pub const API_KEY_SETTING: &str = "nexus_api_key";

pub fn nexus(ctx: &Context, out: Out, cmd: NexusCmd) -> Result<()> {
    match cmd {
        NexusCmd::Apikey => apikey(ctx, out),
        NexusCmd::Logout => {
            ctx.db
                .conn
                .execute("DELETE FROM settings WHERE key = ?1", [API_KEY_SETTING])?;
            out.emit(&serde_json::json!({ "logged_out": true }), || {
                println!("Nexus API key removed");
            })
        }
        NexusCmd::Register => register(out),
        NexusCmd::Unregister => {
            let removed = lmm_nexus::xdg::unregister()?;
            out.emit(
                &serde_json::json!({ "removed": removed }),
                || match removed {
                    Some(p) => println!("removed {}", p.display()),
                    None => println!("no handler was registered"),
                },
            )
        }
        NexusCmd::Status => status(ctx, out),
    }
}

/// Load a configured API key, if any.
pub fn api_key(ctx: &Context) -> Result<Option<String>> {
    Ok(ctx.db.setting(API_KEY_SETTING)?)
}

/// Prompt for the key rather than accepting it as an argument: arguments end
/// up in shell history and process listings; a prompt does not.
fn apikey(ctx: &Context, out: Out) -> Result<()> {
    let key = if std::io::stdin().is_terminal() {
        rpassword::prompt_password("Nexus API key (nexusmods.com → account settings → API keys): ")?
    } else {
        // Piped input (e.g. from a secret manager): read one line.
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        line
    };
    let key = key.trim().to_string();
    if key.is_empty() {
        bail!("no key entered");
    }

    // Validate before storing — a typo'd key should fail here, not later.
    let user = NexusClient::new(key.clone())?.validate_user()?;
    ctx.db.set_setting(API_KEY_SETTING, &key)?;
    out.emit(&user, || {
        println!(
            "hello {} — key {} stored ({} account)",
            user.name,
            mask_key(&key),
            if user.is_premium { "premium" } else { "free" }
        );
    })
}

fn register(out: Out) -> Result<()> {
    let reg = lmm_nexus::xdg::register()?;
    out.emit(&reg, || {
        println!("handler installed: {}", reg.desktop_file.display());
        if reg.mime_default_set {
            println!(
                "nxm:// links are now routed to lmm — use \"Mod Manager Download\" on Nexus Mods"
            );
        }
        for note in &reg.notes {
            println!("note: {note}");
        }
    })
}

#[derive(Serialize)]
struct StatusReport {
    key_configured: bool,
    key_masked: Option<String>,
    account: Option<lmm_nexus::api::User>,
    account_error: Option<String>,
    handler: lmm_nexus::xdg::HandlerStatus,
}

fn status(ctx: &Context, out: Out) -> Result<()> {
    let key = api_key(ctx)?;
    let (account, account_error) = match &key {
        None => (None, None),
        Some(k) => match NexusClient::new(k.clone())?.validate_user() {
            Ok(u) => (Some(u), None),
            Err(e) => (None, Some(e.to_string())),
        },
    };
    let report = StatusReport {
        key_configured: key.is_some(),
        key_masked: key.as_deref().map(mask_key),
        account,
        account_error,
        handler: lmm_nexus::xdg::status()?,
    };
    out.emit(&report, || {
        match (&report.key_masked, &report.account, &report.account_error) {
            (None, ..) => println!("API key:      not set (run 'nexus apikey')"),
            (Some(m), Some(u), _) => println!(
                "API key:      {m} — {} ({} account)",
                u.name,
                if u.is_premium { "premium" } else { "free" }
            ),
            (Some(m), None, Some(e)) => println!("API key:      {m} — validation failed: {e}"),
            (Some(m), None, None) => println!("API key:      {m}"),
        }
        let h = &report.handler;
        if h.desktop_file_exists {
            println!("nxm handler:  {}", h.desktop_file.display());
        } else {
            println!("nxm handler:  not registered (run 'nexus register')");
        }
        match &h.current_handler {
            Some(id) if id == lmm_nexus::xdg::DESKTOP_ID => {
                println!("nxm default:  lmm (clicks on \"Mod Manager Download\" reach lmm)");
            }
            Some(id) => println!("nxm default:  {id} (not lmm; run 'nexus register')"),
            None => println!("nxm default:  none (run 'nexus register')"),
        }
    })
}
