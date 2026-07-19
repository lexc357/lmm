//! Command execution layer.
//!
//! [`dispatch`] is the single entry point for running a parsed [`Command`],
//! no matter where it was parsed from: `lmm <command>` on the command line
//! and a line typed into the interactive shell both end up here. There is no
//! shell-specific command logic — the shell only differs in *how* it parses
//! (a readline line instead of argv) and in the [`Runtime`] it passes.

pub mod deploy;
pub mod downloads;
pub mod fomod;
pub mod games;
pub mod mods;
pub mod nexus;
pub mod nxm;
pub mod profile;
pub mod status;
pub mod tools;

use std::sync::Arc;

use anyhow::Result;
use lmm_core::{Context, Overrides};

use crate::args::Command;
use crate::output::Out;

/// How the current invocation runs — the only thing that distinguishes the
/// interactive shell from a one-shot CLI call at execution time.
#[derive(Clone)]
pub struct Runtime {
    /// File-location overrides, so background workers can open their own
    /// database connections (SQLite connections are not shared across threads).
    pub overrides: Overrides,
    /// Where asynchronous messages (download finished/failed, incoming nxm
    /// links) go: the shell's external printer, or stderr for one-shot runs.
    pub notify: Arc<dyn Fn(String) + Send + Sync>,
    /// True inside the interactive shell: long work (downloads) goes to
    /// background threads and nxm links are handled in-process instead of
    /// being forwarded over the socket.
    pub in_shell: bool,
}

impl Runtime {
    /// Runtime for a one-shot CLI invocation.
    pub fn oneshot(overrides: Overrides) -> Runtime {
        Runtime {
            overrides,
            notify: Arc::new(|msg| eprintln!("{msg}")),
            in_shell: false,
        }
    }
}

/// Execute one parsed command against an open context.
pub fn dispatch(
    ctx: &Context,
    out: Out,
    game_sel: Option<&str>,
    command: Command,
    rt: &Runtime,
) -> Result<()> {
    match command {
        Command::Scan { all } => games::scan(ctx, out, all),
        Command::Game(sub) => games::game(ctx, out, sub),
        Command::Use { install } => games::game(ctx, out, crate::args::GameCmd::Use { install }),
        Command::Install {
            archive,
            name,
            version,
            manual,
        } => mods::install(ctx, out, game_sel, &archive, name, version, manual),
        Command::Mods => mods::list(ctx, out, game_sel),
        Command::Enable { mods } => mods::set_enabled(ctx, out, game_sel, &mods, true),
        Command::Disable { mods } => mods::set_enabled(ctx, out, game_sel, &mods, false),
        Command::Order { r#mod, position } => mods::order(ctx, out, game_sel, &r#mod, position),
        Command::Uninstall { r#mod } => mods::uninstall(ctx, out, game_sel, &r#mod),
        Command::Conflicts => mods::conflicts(ctx, out, game_sel),
        Command::Deploy { dry_run, force } => deploy::deploy(ctx, out, game_sel, dry_run, force),
        Command::Purge { dry_run, force } => deploy::purge(ctx, out, game_sel, dry_run, force),
        Command::Verify => deploy::verify(ctx, out, game_sel),
        Command::Repair { dry_run, force } => deploy::repair(ctx, out, game_sel, dry_run, force),
        Command::Rollback => deploy::rollback(ctx, out, game_sel),
        Command::Profile(sub) => profile::profile(ctx, out, game_sel, sub),
        Command::Status => status::status(ctx, out, game_sel),
        Command::Launch => games::launch(ctx, out, game_sel),
        Command::Nxm { url } => nxm::handle(ctx, out, &url, rt),
        Command::Nexus(sub) => nexus::nexus(ctx, out, sub),
        Command::Downloads { cmd } => downloads::downloads(ctx, out, game_sel, cmd, rt),
        Command::Fomod(sub) => fomod::fomod_cmd(ctx, out, game_sel, sub),
        Command::Tools { cmd } => tools::tools(ctx, out, game_sel, cmd),
    }
}
