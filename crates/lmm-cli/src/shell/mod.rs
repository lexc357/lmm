//! The interactive shell — lmm's primary interface.
//!
//! Design in one paragraph: the shell is a thin readline loop around the
//! exact same clap parser and `cmd::dispatch` layer the one-shot CLI uses.
//! A typed line is split into words (shlex), parsed as if it were `lmm
//! <words>` argv, and dispatched; there is no shell-only command logic. The
//! shell owns three long-lived things the one-shot CLI doesn't have: a
//! reedline editor (history, line editing, completion), an external printer
//! that background threads use to write above the prompt, and a unix-socket
//! listener that receives nxm:// links from browser clicks while the shell
//! runs (see `lmm-nexus/src/ipc.rs`).
//!
//! Completion is split in three: [`complete`] is the pure engine (tokenize,
//! resolve, rank, quote), [`data`] snapshots the candidate state once per
//! prompt, and the small adapters at the bottom of this file translate the
//! engine's results into reedline suggestions and inline hints. Behavior is
//! configured by `[shell.autocomplete]` in the config file.
//!
//! Threading model:
//! - main thread: readline loop and command execution (including
//!   confirmations, which read stdin between readline calls);
//! - one listener thread: accepts nxm links, queues them (fast, local DB
//!   write), acknowledges, then resolves metadata over the network;
//! - one short-lived worker thread per active download.
//!
//! Every thread opens its own SQLite connection (WAL journal + busy timeout
//! make concurrent access safe); they communicate only through the database
//! and the external printer. The shell never blocks on any of them.

use std::borrow::Cow;
use std::io::IsTerminal;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use clap::{CommandFactory, Parser};
use lmm_core::{Context, Overrides, installs, mods, profile};
use lmm_nexus::queue;
use reedline::{
    ColumnarMenu, Emacs, ExternalPrinter, FileBackedHistory, KeyCode, KeyModifiers, MenuBuilder,
    Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, Signal, Span, Suggestion, default_emacs_keybindings,
};

mod complete;
mod data;

use crate::args::Args;
use crate::cmd::{self, Runtime};
use crate::output::Out;

/// Words that end the shell. Checked against the whole (trimmed) line so
/// they can never collide with real command parsing.
const QUIT_WORDS: &[&str] = &["q", "quit", "exit"];

/// Name shared by the completion menu and the Tab keybinding that opens it.
const MENU_NAME: &str = "completion_menu";

pub fn run(overrides: &Overrides, out: Out) -> Result<()> {
    let ctx = Context::open(overrides)?;

    // Non-interactive stdin (a pipe or file): run each line through the same
    // eval path, without readline. `echo status | lmm` just works.
    if !std::io::stdin().is_terminal() {
        return run_batch(&ctx, overrides, out);
    }

    let ac = &ctx.config.shell.autocomplete;
    let opts = complete::Options {
        fuzzy: ac.fuzzy_matching,
        descriptions: ac.show_descriptions,
    };
    // Candidate state for completion, shared with the editor-owned adapters
    // and refreshed once per prompt. If it can't open, the shell still runs —
    // only dynamic completion is lost.
    let completion = match data::CompletionData::new(overrides) {
        Ok(d) => Some(Arc::new(Mutex::new(d))),
        Err(e) => {
            eprintln!("warning: completion unavailable: {e}");
            None
        }
    };

    let history_path = ctx.paths.data_dir.join("shell_history");
    let history = FileBackedHistory::with_file(1000, history_path)?;

    // Background threads print through reedline so messages appear above
    // the prompt instead of garbling the line being edited.
    let printer = ExternalPrinter::<String>::new(256);
    let sender = printer.sender();
    let notify: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |msg: String| {
        // try_send: a notification must never block its worker, and losing
        // one to a full queue only costs a status line.
        let _ = sender.try_send(msg);
    });

    let mut editor = Reedline::create()
        .with_history(Box::new(history))
        .with_external_printer(printer);
    if let Some(data) = &completion {
        if ac.enabled {
            let mut keybindings = default_emacs_keybindings();
            keybindings.add_binding(
                KeyModifiers::NONE,
                KeyCode::Tab,
                ReedlineEvent::UntilFound(vec![
                    ReedlineEvent::Menu(MENU_NAME.to_string()),
                    ReedlineEvent::MenuNext,
                ]),
            );
            editor = editor
                .with_completer(Box::new(EngineCompleter {
                    data: data.clone(),
                    opts,
                }))
                .with_menu(ReedlineMenu::EngineCompleter(Box::new(
                    ColumnarMenu::default().with_name(MENU_NAME),
                )))
                // A single match inserts directly; the menu only opens when
                // there is something to choose. Common prefixes fill in first.
                .with_quick_completions(true)
                .with_partial_completions(true)
                .with_edit_mode(Box::new(Emacs::new(keybindings)));
        }
        if ac.inline_suggestion {
            editor = editor.with_hinter(Box::new(EngineHinter {
                data: data.clone(),
                opts,
                current: String::new(),
            }));
        }
    }

    let rt = Runtime {
        overrides: overrides.clone(),
        notify: notify.clone(),
        in_shell: true,
    };

    println!(
        "Linux Mod Manager v{} — 'help' lists commands, 'q' quits",
        env!("CARGO_PKG_VERSION")
    );
    startup_housekeeping(&ctx);
    let listener_socket = start_nxm_listener(&ctx, overrides, &notify);

    loop {
        // Commands execute between prompts, so refreshing here keeps
        // completion current without any per-keystroke queries.
        if let Some(data) = &completion
            && let Ok(mut d) = data.lock()
        {
            d.refresh();
        }
        let prompt = ShellPrompt {
            text: prompt_text(&ctx),
        };
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                let line = line.trim().to_string();
                if line.is_empty() {
                    continue;
                }
                if QUIT_WORDS.contains(&line.as_str()) {
                    break;
                }
                eval(&ctx, out, &rt, &line);
            }
            // Ctrl-C cancels the current line, not the shell.
            Ok(Signal::CtrlC) => println!("(type 'q' to quit)"),
            // Ctrl-D on an empty line quits.
            Ok(Signal::CtrlD) => break,
            // Nothing emits ExecuteHostCommand, so no other signal arrives.
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
    }

    if let Err(e) = editor.sync_history() {
        eprintln!("warning: could not save shell history: {e}");
    }
    // The listener thread never finishes; clean its socket up ourselves so
    // the next start binds without having to reclaim a stale file.
    if let Some(path) = listener_socket {
        let _ = std::fs::remove_file(path);
    }
    Ok(())
}

/// Parse and execute one line. Errors are printed, never fatal: a typo must
/// not end the session.
fn eval(ctx: &Context, out: Out, rt: &Runtime, line: &str) {
    let Some(words) = shlex::split(line) else {
        eprintln!("error: unbalanced quotes");
        return;
    };
    if words.is_empty() {
        return;
    }

    // Same parser as the command line: the line is argv without the "lmm".
    let parsed = Args::try_parse_from(std::iter::once("lmm".to_string()).chain(words));
    let args = match parsed {
        Ok(args) => args,
        // clap renders its own output for --help/--version/errors.
        Err(e) => {
            let _ = e.print();
            return;
        }
    };

    // File locations were fixed when the shell opened its context.
    if args.config.is_some() || args.data_dir.is_some() || args.db.is_some() {
        eprintln!("note: --config/--data-dir/--db cannot change inside the shell; ignored");
    }
    let Some(command) = args.command else {
        // A bare global flag like `--json` parsed but does nothing alone.
        let _ = Args::command().print_help();
        return;
    };
    let line_out = Out {
        json: args.json || out.json,
        verbose: args.verbose || out.verbose,
        yes: args.yes || out.yes,
    };
    if let Err(e) = cmd::dispatch(ctx, line_out, args.game.as_deref(), command, rt) {
        eprintln!("error: {e:#}");
    }
}

/// Non-TTY mode: evaluate stdin line by line (comments and blanks skipped).
fn run_batch(ctx: &Context, overrides: &Overrides, out: Out) -> Result<()> {
    let rt = Runtime {
        overrides: overrides.clone(),
        notify: Arc::new(|msg| eprintln!("{msg}")),
        in_shell: false, // no prompt to keep responsive; run work inline
    };
    // "Process spooled requests at the next start" applies to every start,
    // not only interactive ones.
    startup_housekeeping(ctx);
    for line in std::io::stdin().lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if QUIT_WORDS.contains(&line) {
            break;
        }
        eval(ctx, out, &rt, line);
    }
    Ok(())
}

/// `lmm`, `lmm [skyrimse]` or `lmm [skyrimse:vanilla-plus]`, reflecting the
/// current default installation and its active profile (shown only when it
/// isn't the implicit "default"). The `> ` indicator is rendered separately
/// by [`ShellPrompt`].
fn prompt_text(ctx: &Context) -> String {
    let Ok(inst) = installs::select(&ctx.db, None) else {
        return "lmm".into();
    };
    let game = inst.label.clone().unwrap_or_else(|| inst.game_slug.clone());
    let active = mods::active_profile_id(ctx, &inst).ok().and_then(|pid| {
        profile::list(&ctx.db, inst.id)
            .ok()?
            .into_iter()
            .find(|p| p.id == pid)
    });
    match active {
        Some(p) if p.name != "default" => format!("lmm [{game}:{}]", p.name),
        _ => format!("lmm [{game}]"),
    }
}

/// One-time startup work: report downloads that died with a previous
/// instance, absorb links spooled while nothing was running, and point at
/// anything waiting for a decision.
fn startup_housekeeping(ctx: &Context) {
    match queue::reset_stale_active(&ctx.db) {
        Ok(n) if n > 0 => {
            println!("note: {n} download(s) were interrupted by a previous exit; see 'downloads'");
        }
        Ok(_) => {}
        Err(e) => eprintln!("warning: could not check for stale downloads: {e}"),
    }

    match cmd::downloads::drain_spool(ctx, |msg| println!("{msg}")) {
        Ok(_) => {}
        Err(e) => eprintln!("warning: could not read the nxm spool: {e}"),
    }

    if let Ok(pending) = queue::list(&ctx.db, Some(queue::Status::Pending))
        && !pending.is_empty()
    {
        println!(
            "{} download(s) waiting for confirmation; list with 'downloads', start with 'downloads start <id>'",
            pending.len()
        );
    }
}

/// Bind the nxm socket and serve it on a background thread. Returns the
/// socket path (for cleanup at exit) or None if unavailable.
///
/// Per link: validate + enqueue (local, fast), acknowledge the browser
/// handler, notify the user, then resolve mod names over the network and
/// notify again. Nothing is downloaded — starting is always explicit.
fn start_nxm_listener(
    ctx: &Context,
    overrides: &Overrides,
    notify: &Arc<dyn Fn(String) + Send + Sync>,
) -> Option<std::path::PathBuf> {
    let listener = match lmm_nexus::ipc::NxmListener::bind(&ctx.paths) {
        Ok(l) => l,
        Err(lmm_nexus::Error::AlreadyListening) => {
            println!("note: another lmm instance is receiving nxm links; this one will not");
            return None;
        }
        Err(e) => {
            eprintln!("warning: nxm link listener unavailable: {e}");
            return None;
        }
    };
    let socket_path = listener.path().to_path_buf();

    let overrides = overrides.clone();
    let notify = notify.clone();
    std::thread::spawn(move || {
        // The listener thread has its own database connection for enqueuing.
        let ctx = match Context::open(&overrides) {
            Ok(c) => c,
            Err(e) => {
                notify(format!("nxm listener stopped: {e}"));
                return;
            }
        };
        listener.serve(|raw| match cmd::downloads::ingest_link_quick(&ctx, raw) {
            Ok((d, fresh)) => {
                if fresh {
                    notify(format!(
                        "nxm: queued {} as download {} — start with 'downloads start {}'",
                        d.describe(),
                        d.id,
                        d.id
                    ));
                    if d.mod_name.is_none() {
                        resolve_in_background(&overrides, d.id, &notify);
                    }
                } else {
                    notify(format!("nxm: download {} already {}", d.id, d.status));
                }
                Ok(format!("queued as download {}", d.id))
            }
            Err(e) => {
                notify(format!("nxm: rejected incoming link: {e:#}"));
                Err(format!("{e:#}"))
            }
        });
    });
    Some(socket_path)
}

/// Resolve a queued download's mod/file names on a short-lived thread so
/// the listener can go back to accepting links immediately.
fn resolve_in_background(
    overrides: &Overrides,
    id: i64,
    notify: &Arc<dyn Fn(String) + Send + Sync>,
) {
    let overrides = overrides.clone();
    let notify = notify.clone();
    std::thread::spawn(move || {
        let Ok(ctx) = Context::open(&overrides) else {
            return;
        };
        let Ok(d) = queue::get(&ctx.db, id) else {
            return;
        };
        match cmd::downloads::resolve_metadata(&ctx, &d) {
            Ok(d) => notify(format!("nxm: download {} is {}", d.id, d.describe())),
            Err(e) => notify(format!(
                "nxm: could not resolve names for download {id}: {e:#}"
            )),
        }
    });
}

// ---------------------------------------------------------------------------
// Reedline adapters: prompt, completer, hinter. All logic lives in
// `complete` and `data`; these only translate types.

/// The two-part prompt: contextual text ("lmm [skyrimse]") plus a fixed
/// `> ` indicator. Rebuilt before every read so it tracks the default
/// installation and active profile.
struct ShellPrompt {
    text: String,
}

impl Prompt for ShellPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed(&self.text)
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_indicator(&self, _mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("> ")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed(":: ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let status = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({status}reverse-search: {}) ",
            history_search.term
        ))
    }
}

/// Feeds engine completions to reedline's Tab menu.
struct EngineCompleter {
    data: Arc<Mutex<data::CompletionData>>,
    opts: complete::Options,
}

impl reedline::Completer for EngineCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> Vec<Suggestion> {
        let Ok(data) = self.data.lock() else {
            return Vec::new();
        };
        let completions = complete::complete(data.snapshot(), self.opts, line, pos);
        completions
            .items
            .into_iter()
            .map(|item| Suggestion {
                value: item.insert,
                display_override: Some(item.display),
                description: item.description,
                span: Span::new(completions.start, pos),
                append_whitespace: item.append_space,
                ..Suggestion::default()
            })
            .collect()
    }
}

/// Shows the engine's single-candidate hint inline, faded, after the cursor.
struct EngineHinter {
    data: Arc<Mutex<data::CompletionData>>,
    opts: complete::Options,
    /// Last hint, unstyled, so accepting it inserts exactly this text.
    current: String,
}

impl reedline::Hinter for EngineHinter {
    fn handle(
        &mut self,
        line: &str,
        pos: usize,
        _history: &dyn reedline::History,
        use_ansi_coloring: bool,
        _cwd: &str,
    ) -> String {
        self.current = self
            .data
            .lock()
            .ok()
            .and_then(|d| complete::hint(d.snapshot(), self.opts, line, pos))
            .unwrap_or_default();
        if self.current.is_empty() || !use_ansi_coloring {
            self.current.clone()
        } else {
            nu_ansi_term::Style::new()
                .fg(nu_ansi_term::Color::DarkGray)
                .paint(&self.current)
                .to_string()
        }
    }

    fn complete_hint(&self) -> String {
        self.current.clone()
    }

    fn next_hint_token(&self) -> String {
        // The first word (with any leading whitespace), for partial accepts.
        let mut token = String::new();
        let mut in_word = false;
        for c in self.current.chars() {
            if c.is_whitespace() && in_word {
                break;
            }
            in_word = in_word || !c.is_whitespace();
            token.push(c);
        }
        token
    }
}
