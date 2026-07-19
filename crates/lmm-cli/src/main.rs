#![cfg_attr(test, allow(clippy::unwrap_used))]

mod args;
mod cmd;
mod output;
mod shell;

use clap::Parser;
use lmm_core::{Context, Overrides};

use crate::args::{Args, Command};
use crate::cmd::Runtime;
use crate::output::Out;

fn main() {
    let args = Args::parse();
    let out = Out {
        json: args.json,
        verbose: args.verbose,
        yes: args.yes,
    };
    if let Err(e) = run(args, out) {
        // `lmm --json ... | head` closing stdout early is not an error.
        if is_broken_pipe(&e) {
            std::process::exit(0);
        }
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn is_broken_pipe(e: &anyhow::Error) -> bool {
    e.chain().any(|cause| {
        let io_kind = cause
            .downcast_ref::<std::io::Error>()
            .map(std::io::Error::kind)
            .or_else(|| {
                cause
                    .downcast_ref::<serde_json::Error>()
                    .and_then(serde_json::Error::io_error_kind)
            });
        io_kind == Some(std::io::ErrorKind::BrokenPipe)
    })
}

fn run(args: Args, out: Out) -> anyhow::Result<()> {
    let overrides = Overrides {
        config_path: args.config.clone(),
        data_dir: args.data_dir.clone(),
        db_path: args.db.clone(),
    };

    // No command: start the interactive shell (the primary interface).
    let Some(command) = args.command else {
        return shell::run(&overrides, out);
    };

    let rt = Runtime::oneshot(overrides.clone());
    let ctx = match Context::open(&overrides) {
        Ok(ctx) => ctx,
        // The nxm browser handler must never lose a click: if the database
        // (or config) is unusable, validate the link and spool it to disk so
        // the next healthy start picks it up.
        Err(open_err) => {
            if let Command::Nxm { url } = &command {
                return spool_link_without_context(&overrides, out, url, &open_err);
            }
            return Err(open_err.into());
        }
    };
    out.verbose(format!("data dir: {}", ctx.paths.data_dir.display()));
    cmd::dispatch(&ctx, out, args.game.as_deref(), command, &rt)
}

/// Last-resort nxm delivery when `Context::open` failed: validate the link,
/// then store it in the spool directory next to where the data dir should
/// be. Only the paths are computed; the database is never touched.
fn spool_link_without_context(
    overrides: &Overrides,
    out: Out,
    url: &str,
    open_err: &lmm_core::error::Error,
) -> anyhow::Result<()> {
    let link = lmm_nexus::nxm::NxmLink::parse(url)?;
    let data_dir = match &overrides.data_dir {
        Some(d) => d.clone(),
        None => lmm_core::config::default_data_dir()?,
    };
    let paths = lmm_core::config::DataPaths::new(data_dir, overrides.db_path.clone());
    paths.ensure_dirs()?;
    lmm_nexus::ipc::spool(&paths, url)?;
    out.emit(
        &serde_json::json!({ "spooled": true, "link": link.to_string() }),
        || {
            println!("lmm could not open its database ({open_err}); stored {link} — it will be queued on the next start");
        },
    )
}
