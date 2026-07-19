# The interactive shell

Since stage 9, running `lmm` with no arguments starts an interactive shell,
which is the primary interface to the application:

```
$ lmm
Linux Mod Manager v0.1.0 — 'help' lists commands, 'q' quits
lmm> scan
lmm> use skyrimse
lmm [skyrimse]> install ~/Downloads/mod.7z
lmm [skyrimse]> profile switch vanilla-plus
lmm [skyrimse:vanilla-plus]> deploy
lmm [skyrimse:vanilla-plus]> q
```

The shell exits on `q`, `quit`, `exit`, or Ctrl-D. Ctrl-C cancels the current
line, never the session.

## One parser, one execution layer

The design rule for the shell is that **it contains no command logic**. A
typed line is:

1. split into words with shell quoting rules (`shlex`), then
2. parsed by the *same* clap definition (`args::Args`) that parses the real
   command line, as if the words were argv, then
3. executed by the *same* `cmd::dispatch()` the one-shot CLI uses.

```
   argv ("lmm deploy --dry-run")──────────┐
                                          ▼
                                   clap (args::Args)──► cmd::dispatch(ctx, out, cmd, rt)
                                          ▲
   readline line ("deploy --dry-run")─────┘
        (shlex → argv)
```

Consequences that fall out of this for free:

* every command works identically in both interfaces, including flags
  (`deploy --dry-run`, `--json`, `--game`, `--yes`);
* `help`, `<command> --help`, and clap's usage errors work inside the shell;
* tab completion of command names is *derived from* the clap definition
  (`Args::command().get_subcommands()`), so it cannot drift from the parser.

The only inputs the shell treats specially are the quit words (`q`, `quit`,
`exit`) — session lifecycle, not commands — and the file-location flags
(`--config`, `--data-dir`, `--db`), which are fixed for the session because
the context is already open (the shell says so if you pass them).

The difference between interfaces is captured in one small struct,
`cmd::Runtime`: where asynchronous messages go (external printer vs stderr)
and whether long work may go to background threads (`in_shell`).

## The event loop

`shell::run` (crates/lmm-cli/src/shell.rs):

1. open the `Context` (config + database), same as any CLI invocation;
2. **startup housekeeping**: mark downloads left `active` by a dead instance
   as failed, drain the nxm spool (see docs/NEXUS.md), and mention pending
   downloads;
3. bind the nxm listener socket and spawn the listener thread;
4. loop: build the prompt from current state, `readline`, evaluate.

The prompt is recomputed every iteration from the database — it is a *view*,
not shell state:

```
lmm>                          no usable default installation
lmm [skyrimse]>               default installation (label or game slug)
lmm [skyrimse:vanilla-plus]>  …with a non-"default" active profile
```

`use <install>` is therefore not shell magic either: it is a real command
(alias of `game use`) that sets the default installation in the database; the
next prompt simply reflects it. Running `use` from the plain CLI does exactly
the same thing.

Line editing, history (persisted to `<data-dir>/shell_history`), arrow keys,
the Tab menu and inline hints come from reedline. Completion itself is lmm's
own engine, split in three so it stays testable without a terminal:

* `shell/complete.rs` — the pure engine. It tokenizes the line up to the
  cursor with the same quoting rules the parser (shlex) uses, resolves what
  the word *is* by walking the clap command tree (command, subcommand, flag,
  or a positional of a declared kind — see `args::positional_kind`), gathers
  candidates, ranks them in tiers (exact prefix → case-insensitive →
  normalized → word prefix → optional fuzzy), and quotes insertions so a
  completed value can never split into several arguments.
* `shell/data.rs` — a snapshot of the candidate state (installations, mods
  with enabled/disabled, profiles, downloads by status), refreshed once per
  prompt on a dedicated database connection. State only changes between
  prompts, so the snapshot is always current with zero per-keystroke queries.
* thin adapters in `shell/mod.rs` — translate engine results into reedline
  suggestions (Tab menu, with descriptions) and inline hints (shown faded
  after the cursor when exactly one candidate matches; Right-arrow accepts).

Position awareness covers command and subcommand names (from clap), flags,
file paths (for `install` / `game add`), mod names filtered by state (only
disabled mods after `enable`, …), installation selectors, profile names, and
download ids filtered by status per subcommand. `[shell.autocomplete]` in the
config file controls it: `enabled` (Tab menu), `inline_suggestion`,
`fuzzy_matching`, and `show_descriptions`, all on by default.

If stdin is not a terminal, `lmm` runs the same evaluator over stdin line by
line (batch mode): `echo status | lmm` works in scripts, `#` comments and
blank lines are skipped, and readline never starts.

## Background work

Threading model — three kinds of threads, communicating only through the
database and the external printer:

| Thread | Count | Job |
|---|---|---|
| main | 1 | readline loop, command execution, confirmations |
| nxm listener | 0..1 | accept links from browser clicks, enqueue, ack |
| download worker | 1 per active download | transfer bytes, update progress |

Rules that keep this simple and safe:

* **Every thread opens its own SQLite connection** (via the same `Overrides`)
  — connections are never shared. WAL journaling plus a 5s busy timeout make
  concurrent readers/writers safe; the queue table's guarded `UPDATE ...
  WHERE status = ...` transitions make double-starts impossible.
* **Notifications go through reedline's external printer**, so a "download
  complete" message from a worker is drawn *above* the line being edited
  instead of garbling it. One-shot invocations use plain stderr through the
  same `Runtime::notify` interface.
* **Cancellation runs through the database.** `downloads cancel` flips the
  row; the worker's next progress heartbeat (`queue::set_progress`) sees the
  row is no longer active and abandons the transfer. No thread ever needs a
  handle to another.

Workers are deliberately not joined at exit: they own no shared state other
than the database row they update, and a row left `active` by an aborted
process is reset to failed by the next startup housekeeping.
