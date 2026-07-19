# Nexus Mods (nxm://) integration

Stage 9 adds Linux-native support for the "Mod Manager Download" button on
nexusmods.com. Clicking it makes the browser open an `nxm://` URL; lmm
registers itself as the desktop handler for that scheme, receives the link,
and turns it into a row in a persistent download queue. Nothing is ever
downloaded, installed, or deployed because a browser said so — every transfer
starts with an explicit `downloads start`, and installation/deployment stay
the separate steps they always were.

All of the non-interface logic lives in the `lmm-nexus` crate; the `nexus`,
`nxm` and `downloads` commands in `lmm-cli` are thin frontends over it.

## Link delivery

`nexus register` writes `~/.local/share/applications/lmm-nxm.desktop`
(`Exec=lmm nxm %u`, `MimeType=x-scheme-handler/nxm`) and runs
`xdg-mime default lmm-nxm.desktop x-scheme-handler/nxm`. From then on every
click spawns a short-lived `lmm nxm <url>` process, which must get the link
to wherever it will be handled:

```
browser click ──► lmm nxm <url>  (validate first, always)
                     │
                     ├─ shell running? ──── unix socket ───► listener thread:
                     │                                       enqueue → ack →
                     │                                       notify + resolve
                     ├─ no shell, db ok ─── enqueue directly (status: pending)
                     │
                     └─ db unusable ─────── spool file; drained & enqueued
                                            at the next lmm start
```

* **Socket** — `$XDG_RUNTIME_DIR/lmm-<hash>.sock` (falls back to the data
  dir), where `<hash>` is derived from the data dir so separate lmm setups
  never cross-talk. Protocol: one `NXM <url>` line in, one `OK …`/`ERR …`
  line back, both length-capped, 5s timeouts. The shell's listener thread
  enqueues the link locally (no network) before acknowledging, so the
  handler process returns to the browser immediately; mod/file names are
  resolved right after on a short-lived thread and reported via a shell
  notification. A stale socket left by a crash is detected (connect fails)
  and reclaimed; a second shell instance notices a live listener and says
  links go to the first one.
* **Spool** — one file per request under `<data-dir>/nxm-spool/`, mode 0600
  (the URL embeds a download key). Both shell and batch starts drain it:
  each entry is re-validated, enqueued, reported, and deleted — a request
  survives any crash between click and processing, and garbage in the spool
  is dropped rather than reprocessed forever.

## The download queue

One SQLite table (`downloads`, migration v2), shared by the shell, one-shot
CLI commands and worker threads — it is the single source of truth and the
only channel between them:

```
nxm link ──► pending ──('downloads start')──► active ──► completed ──('downloads install')─► normal
              ▲                                 │                                             install
              └──────── retry / new link ─── failed ◄── cancel / crash / error               pipeline
```

* `downloads` lists everything; `start <id>|--all` is the user confirmation
  that begins a transfer (in the shell: background worker per download; as a
  one-shot command: foreground with a progress line).
* Queueing is idempotent: re-clicking the button refreshes the existing
  pending/failed row with the new download key instead of duplicating it —
  which is also exactly how an expired key is renewed.
* Cancellation flips the row in the database; the worker notices at its next
  progress write. Rows left `active` by a crash are failed at next startup.
* A completed row records the archive path, size, and SHA-256.
  `downloads install <id>` re-hashes the file, refuses on mismatch, routes to
  the right installation via the link's game domain (e.g.
  `skyrimspecialedition` → the `skyrimse` installation, overridable with
  `--game`), and then calls the *same* install code path as
  `install <archive>` — validation, staging and deployment are unchanged.

## Nexus API

`lmm-nexus/src/api.rs` speaks to `api.nexusmods.com/v1` with the user's
personal API key (`nexus apikey` prompts for it — never a command-line
argument — validates it against `/users/validate.json`, and stores it in the
settings table). Endpoints used: mod info, file info, and
`download_link.json`, which converts the link's short-lived `key`/`expires`
pair into signed CDN URLs (this is the non-premium flow; premium keys work
without the pair). A 403 is surfaced as "link expired — click the button
again", which is also the recovery path.

## Threat model

Everything that enters this feature is untrusted: nxm URLs (any process can
write to the socket; any website can trigger the handler), API responses, and
the downloaded bytes.

* **Links** are strictly validated at every entry point (`NxmLink::parse`):
  exact scheme, alphanumeric game domain, exact `/mods/<id>/files/<id>`
  shape, bounded lengths. The socket listener re-validates — it never trusts
  the sending process.
* **Credentials are contained.** The API key is only ever sent to
  api.nexusmods.com and shown masked; the nxm `key` is excluded from
  `Display`, from `--json` output (`#[serde(skip)]`), and is deleted from the
  row once the download completes; signed CDN URLs are used for the one
  request and never stored or shown. Error messages are built from status
  codes and operation names, never URLs.
* **Downloads are bounded and atomic**: https-only mirrors, a hard size cap
  (config `[limits] max_file_size_mib`) enforced on the stream regardless of
  headers, hashing on the fly, temp file + fsync + rename so a crash never
  leaves a plausible-looking partial archive.
* **File names from the API are sanitized** (final path component only, safe
  character set, bounded length, no leading dots) and created only inside
  the downloads directory, with collision-proof naming.
* **The game directory is never touched by any of this.** A download ends as
  an ordinary archive file; from there on the existing pipeline's guarantees
  (archive validation, staging, journaled deployment, backups) apply
  unchanged.
