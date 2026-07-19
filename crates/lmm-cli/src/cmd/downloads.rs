//! `downloads ...` — the Nexus download queue.
//!
//! This module owns the whole download lifecycle on the CLI side:
//!
//! - [`ingest_link`] turns a raw nxm link into a pending queue row (used by
//!   the `nxm` command, the shell's socket listener and the spool drain);
//! - `downloads start` is the explicit user confirmation that actually
//!   transfers bytes — nothing downloads without it;
//! - [`run_one_download`] performs a single transfer. In the shell it runs
//!   on a background thread with its own database connection so the prompt
//!   stays responsive; as a one-shot CLI command it runs in the foreground
//!   with a progress line.
//!
//! Downloading and installing stay separate: a completed download just sits
//! in the downloads directory until `downloads install <id>` (or a plain
//! `install <path>`) pushes it through the normal archive import pipeline.

use std::path::PathBuf;

use anyhow::{Context as _, Result, bail};
use lmm_core::{Context, Overrides, games};
use lmm_nexus::api::NexusClient;
use lmm_nexus::nxm::NxmLink;
use lmm_nexus::queue::{self, Download, Status};
use lmm_nexus::{download, ipc};

use crate::args::DownloadsCmd;
use crate::cmd::Runtime;
use crate::cmd::nexus::api_key;
use crate::output::{Out, fmt_time, print_table};

pub fn downloads(
    ctx: &Context,
    out: Out,
    game_sel: Option<&str>,
    cmd: Option<DownloadsCmd>,
    rt: &Runtime,
) -> Result<()> {
    match cmd.unwrap_or(DownloadsCmd::List) {
        DownloadsCmd::List => list(ctx, out),
        DownloadsCmd::Start { ids, all } => start(ctx, out, ids, all, rt),
        DownloadsCmd::Cancel { id } => {
            queue::request_cancel(&ctx.db, id)?;
            out.emit(&serde_json::json!({ "cancelled": id }), || {
                println!("cancelled download {id}");
            })
        }
        DownloadsCmd::Retry { id } => retry(ctx, out, id, rt),
        DownloadsCmd::Remove { id } => {
            let d = queue::get(&ctx.db, id)?;
            queue::remove(&ctx.db, id)?;
            out.emit(&serde_json::json!({ "removed": id }), || {
                println!("removed download record {id} ({})", d.describe());
                if let Some(p) = &d.archive_path {
                    println!("the archive stays at {p}");
                }
            })
        }
        DownloadsCmd::Install { id, name } => install(ctx, out, game_sel, id, name),
    }
}

fn list(ctx: &Context, out: Out) -> Result<()> {
    let all = queue::list(&ctx.db, None)?;
    out.emit(&all, || {
        if all.is_empty() {
            println!(
                "no downloads; click \"Mod Manager Download\" on Nexus Mods or pass a link to 'nxm'"
            );
            return;
        }
        let rows: Vec<Vec<String>> = all.iter().map(row).collect();
        print_table(
            &["id", "status", "game", "mod", "file", "size", "note"],
            &rows,
        );
        let pending = all.iter().filter(|d| d.status == Status::Pending).count();
        if pending > 0 {
            println!(
                "\n{pending} pending; start with 'downloads start <id>' or 'downloads start --all'"
            );
        }
    })
}

fn row(d: &Download) -> Vec<String> {
    let size = match (d.status, d.total_bytes) {
        (Status::Active, Some(total)) if total > 0 => {
            format!("{}% of {}", d.bytes_done * 100 / total, fmt_bytes(total))
        }
        (Status::Active, _) => fmt_bytes(d.bytes_done),
        (_, Some(total)) => fmt_bytes(total),
        _ => String::new(),
    };
    vec![
        d.id.to_string(),
        d.status.to_string(),
        d.game_domain.clone(),
        d.mod_name
            .clone()
            .unwrap_or_else(|| format!("mod {}", d.nexus_mod_id)),
        d.file_name
            .clone()
            .unwrap_or_else(|| format!("file {}", d.nexus_file_id)),
        size,
        match d.status {
            Status::Failed => d.error.clone().unwrap_or_default(),
            Status::Completed => fmt_time(d.updated_at),
            _ => String::new(),
        },
    ]
}

pub fn fmt_bytes(n: i64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GiB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MiB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KiB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

/// Validate and queue a raw nxm link without touching the network. This is
/// the fast path the shell's socket listener uses so the browser handler
/// gets its acknowledgement immediately; metadata resolution happens after.
pub fn ingest_link_quick(ctx: &Context, raw: &str) -> Result<(Download, bool)> {
    let link = NxmLink::parse(raw)?;
    Ok(queue::enqueue(&ctx.db, &link)?)
}

/// Validate and queue a raw nxm link, then try (best-effort) to resolve its
/// mod/file names via the API so the user sees what was queued. Returns the
/// row and a one-line human message. Never downloads anything.
pub fn ingest_link(ctx: &Context, raw: &str) -> Result<(Download, String)> {
    let (mut d, fresh) = ingest_link_quick(ctx, raw)?;

    if !fresh {
        return Ok((
            d.clone(),
            format!("download {} already {} ({})", d.id, d.status, d.describe()),
        ));
    }

    // Best-effort metadata resolution: a missing API key or a network error
    // must not lose the request — it stays queued with ids only.
    let mut note = String::new();
    match resolve_metadata(ctx, &d) {
        Ok(resolved) => d = resolved,
        Err(e) => note = format!(" (metadata not resolved: {e:#})"),
    }
    let game = games::by_nexus_domain(&d.game_domain)
        .map(|g| g.name)
        .unwrap_or(d.game_domain.as_str());
    Ok((
        d.clone(),
        format!(
            "queued {} for {game} as download {}{note} — start with 'downloads start {}'",
            d.describe(),
            d.id,
            d.id
        ),
    ))
}

/// Fetch mod/file names and size from the Nexus API and store them.
pub fn resolve_metadata(ctx: &Context, d: &Download) -> Result<Download> {
    let Some(key) = api_key(ctx)? else {
        bail!("no Nexus API key configured; run 'nexus apikey'")
    };
    let client = NexusClient::new(key)?;
    let mod_info = client.mod_info(&d.game_domain, d.nexus_mod_id)?;
    let file_info = client.file_info(&d.game_domain, d.nexus_mod_id, d.nexus_file_id)?;
    queue::set_resolved(
        &ctx.db,
        d.id,
        mod_info.name.as_deref(),
        file_info.file_name.as_deref().or(file_info.name.as_deref()),
        file_info.version.as_deref().or(mod_info.version.as_deref()),
        file_info.size_in_bytes.map(|s| s as i64),
    )?;
    Ok(queue::get(&ctx.db, d.id)?)
}

/// Queue rows spooled while lmm was not running (called once at shell start).
pub fn drain_spool(ctx: &Context, notify: impl Fn(String)) -> Result<usize> {
    let raw_links = ipc::drain_spool(&ctx.paths)?;
    let mut queued = 0;
    for raw in raw_links {
        match ingest_link(ctx, &raw) {
            Ok((_, msg)) => {
                queued += 1;
                notify(format!("nxm (from spool): {msg}"));
            }
            // The spool is written by our own handler, but its contents are
            // still untrusted; a bad entry is reported and dropped.
            Err(e) => notify(format!("nxm (from spool): rejected a stored link: {e:#}")),
        }
    }
    Ok(queued)
}

fn start(ctx: &Context, out: Out, ids: Vec<i64>, all: bool, rt: &Runtime) -> Result<()> {
    let targets: Vec<Download> = if all {
        queue::list(&ctx.db, Some(Status::Pending))?
    } else {
        ids.iter()
            .map(|id| queue::get(&ctx.db, *id))
            .collect::<lmm_nexus::Result<_>>()?
    };
    if targets.is_empty() {
        return out.emit(&serde_json::json!({ "started": [] }), || {
            println!("nothing to start; queue a download by clicking \"Mod Manager Download\"");
        });
    }
    if api_key(ctx)?.is_none() {
        bail!("no Nexus API key configured; run 'nexus apikey' first");
    }

    if rt.in_shell {
        // Background: one worker thread per download, each with its own
        // database connection. Results arrive via rt.notify.
        for d in &targets {
            // Claim the row on this connection first so a typo'd id or a
            // double start fails here, visibly, not inside the thread.
            queue::mark_active(&ctx.db, d.id)?;
            let overrides = rt.overrides.clone();
            let notify = rt.notify.clone();
            let (id, what) = (d.id, d.describe());
            std::thread::spawn(move || match run_one_download(&overrides, id, false) {
                Ok(done) => notify(format!(
                    "download {id} complete: {} ({}) — install with 'downloads install {id}'",
                    what,
                    fmt_bytes(done.size as i64)
                )),
                Err(e) => notify(format!("download {id} failed: {e:#}")),
            });
        }
        let ids: Vec<i64> = targets.iter().map(|d| d.id).collect();
        out.emit(&serde_json::json!({ "started": ids }), || {
            println!(
                "started {} download(s) in the background; watch 'downloads' for progress",
                targets.len()
            );
        })
    } else {
        // One-shot CLI: run sequentially in the foreground.
        for d in &targets {
            queue::mark_active(&ctx.db, d.id)?;
            out.info(format!("downloading {} ...", d.describe()));
            let done = run_one_download(&rt.overrides, d.id, !out.json)?;
            out.emit(&queue::get(&ctx.db, d.id)?, || {
                println!(
                    "done: {} ({}, sha256 {})",
                    done.path.display(),
                    fmt_bytes(done.size as i64),
                    &done.sha256[..12]
                );
            })?;
        }
        Ok(())
    }
}

/// Transfer one queue row that has already been marked active.
///
/// Opens its own [`Context`] so it can run on any thread. All state changes
/// go through the queue table, which is also how cancellation arrives: when
/// `downloads cancel` flips the row, the next progress heartbeat returns
/// false and the transfer stops.
fn run_one_download(
    overrides: &Overrides,
    id: i64,
    show_progress: bool,
) -> Result<download::Downloaded> {
    let ctx = Context::open(overrides)?;
    let result = transfer(&ctx, id, show_progress);
    match &result {
        Ok(done) => queue::mark_completed(
            &ctx.db,
            id,
            &done.path.to_string_lossy(),
            &done.sha256,
            done.size as i64,
        )?,
        // A cancelled row is already 'failed: cancelled'; don't overwrite.
        Err(e) => {
            if queue::get(&ctx.db, id)
                .map(|d| d.status == Status::Active)
                .unwrap_or(false)
            {
                queue::mark_failed(&ctx.db, id, &format!("{e:#}"))?;
            }
        }
    }
    result
}

fn transfer(ctx: &Context, id: i64, show_progress: bool) -> Result<download::Downloaded> {
    let mut d = queue::get(&ctx.db, id)?;

    // Late metadata resolution: a row queued while offline or without an API
    // key may still be name-less.
    if d.file_name.is_none() {
        d = resolve_metadata(ctx, &d)?;
    }

    let link = d.link();
    if link.is_expired(lmm_core::db::now()) {
        bail!(
            "the download key for {} has expired; click \"Mod Manager Download\" on Nexus Mods again",
            d.describe()
        );
    }

    let key = api_key(ctx)?.context("no Nexus API key configured; run 'nexus apikey'")?;
    let client = NexusClient::new(key)?;
    let mirrors = client.download_links(&link)?;

    let file_name = d.file_name.clone().unwrap_or_else(|| {
        format!(
            "{}-{}-{}.bin",
            d.game_domain, d.nexus_mod_id, d.nexus_file_id
        )
    });
    let max_bytes = ctx.config.limits.max_file_size();
    let total = d.total_bytes;

    // First mirror is Nexus's preferred one; that's good enough.
    let done = download::fetch(
        &mirrors[0].uri,
        &ctx.paths.downloads_dir,
        &file_name,
        max_bytes,
        |bytes| {
            if show_progress {
                match total {
                    Some(t) if t > 0 => eprint!(
                        "\r  {} / {} ({}%)   ",
                        fmt_bytes(bytes as i64),
                        fmt_bytes(t),
                        bytes as i64 * 100 / t
                    ),
                    _ => eprint!("\r  {}   ", fmt_bytes(bytes as i64)),
                }
            }
            // Heartbeat doubles as the cancellation check.
            queue::set_progress(&ctx.db, id, bytes as i64).unwrap_or(false)
        },
    );
    if show_progress {
        eprintln!();
    }
    Ok(done?)
}

fn retry(ctx: &Context, out: Out, id: i64, rt: &Runtime) -> Result<()> {
    let d = queue::get(&ctx.db, id)?;
    if d.status != Status::Failed {
        bail!("download {id} is {}, not failed", d.status);
    }
    // Re-enqueue with whatever credential we still have; if the key has
    // expired the start will say so clearly.
    let (d, _) = queue::enqueue(&ctx.db, &d.link())?;
    start(ctx, out, vec![d.id], false, rt)
}

/// Push a completed download through the regular archive import pipeline.
/// Deliberately just a thin wrapper over `install <archive>` — downloads do
/// not get a private install path.
fn install(
    ctx: &Context,
    out: Out,
    game_sel: Option<&str>,
    id: i64,
    name: Option<String>,
) -> Result<()> {
    let d = queue::get(&ctx.db, id)?;
    if d.status != Status::Completed {
        bail!(
            "download {id} is {}; only completed downloads can be installed",
            d.status
        );
    }
    let Some(path) = d.archive_path.as_deref().map(PathBuf::from) else {
        bail!("download {id} has no archive path recorded");
    };
    if !path.exists() {
        bail!(
            "archive {} no longer exists; remove the record with 'downloads remove {id}' \
             and download again",
            path.display()
        );
    }
    // The archive must still be the exact bytes we downloaded and hashed.
    if let Some(expected) = &d.sha256 {
        let actual = lmm_core::hash::sha256_file(&path)?;
        if actual != *expected {
            bail!(
                "{} changed on disk since it was downloaded (sha256 mismatch); refusing to install",
                path.display()
            );
        }
    }

    // --game wins; otherwise route by the link's game domain (e.g. a
    // skyrimspecialedition download goes to the skyrimse installation).
    let mapped = games::by_nexus_domain(&d.game_domain).map(|g| g.slug.to_string());
    let sel = game_sel.map(str::to_string).or(mapped);

    crate::cmd::mods::install(
        ctx,
        out,
        sel.as_deref(),
        &path,
        name.or_else(|| d.mod_name.clone()),
        d.version.clone(),
        false, // FOMOD detection applies to downloads too
    )
}
