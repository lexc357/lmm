//! `nxm <url>` — entry point for Nexus "Mod Manager Download" links.
//!
//! The registered desktop handler runs `lmm nxm <url>` for every click, so
//! this command must work in three situations:
//!
//! 1. **An interactive shell is running** — deliver the link to it over the
//!    unix socket; the shell validates, queues, resolves metadata and shows
//!    a notification. The handler process just relays the reply.
//! 2. **No instance is running, database reachable** — queue the link
//!    directly; the request is processed (started) whenever the user next
//!    looks at their downloads.
//! 3. **Database not even reachable** — handled in `main.rs`, not here: the
//!    link is validated and spooled to disk, and the next lmm start drains
//!    the spool (see `cmd::downloads::drain_spool`).
//!
//! Inside the shell, `nxm <url>` (typed manually, e.g. with a copied link)
//! skips the socket — we *are* the running instance — and queues directly.
//!
//! In every path the link is validated first and the raw URL (which carries
//! a download key) is never echoed back or logged.

use anyhow::{Result, bail};
use lmm_core::Context;
use lmm_nexus::ipc::{self, Delivery};
use lmm_nexus::nxm::NxmLink;

use crate::cmd::Runtime;
use crate::cmd::downloads::ingest_link;
use crate::output::Out;

pub fn handle(ctx: &Context, out: Out, raw: &str, rt: &Runtime) -> Result<()> {
    // Validate before doing anything else; garbage dies here.
    let link = NxmLink::parse(raw)?;

    if !rt.in_shell {
        // Prefer a running instance: it can notify the user immediately.
        match ipc::send_to_running(&ctx.paths, raw)? {
            Delivery::Accepted(ack) => {
                return out.emit(
                    &serde_json::json!({ "delivered": true, "link": link.to_string(), "ack": ack }),
                    || println!("delivered to running lmm: {ack}"),
                );
            }
            Delivery::Rejected(reason) => {
                bail!("running lmm instance rejected the link: {reason}");
            }
            Delivery::NotRunning => {} // fall through to queueing directly
        }
    }

    let (download, msg) = ingest_link(ctx, raw)?;
    out.emit(&download, || println!("{msg}"))
}
