use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "lmm",
    version,
    about = "Linux mod manager: safe, scriptable game modding for Steam/Proton",
    max_term_width = 100
)]
pub struct Args {
    /// With no command, lmm starts its interactive shell.
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Print machine-readable JSON on stdout (diagnostics go to stderr)
    #[arg(long, global = true)]
    pub json: bool,

    /// Verbose diagnostics on stderr
    #[arg(short, long, global = true)]
    pub verbose: bool,

    /// Assume "yes" for all confirmations (required when stdin is not a TTY)
    #[arg(short, long, global = true)]
    pub yes: bool,

    /// Config file path (default: $XDG_CONFIG_HOME/lmm/config.toml)
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Data directory (default: $XDG_DATA_HOME/lmm)
    #[arg(long, global = true, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,

    /// Database path (default: <data-dir>/lmm.db)
    #[arg(long, global = true, value_name = "FILE")]
    pub db: Option<PathBuf>,

    /// Target installation (id, game slug, or label); defaults to the
    /// installation set with 'lmm game use', or the only one registered
    #[arg(short, long, global = true, value_name = "INSTALL")]
    pub game: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Discover Steam games on this machine
    Scan {
        /// List every Steam app found, not only supported games
        #[arg(long)]
        all: bool,
    },
    /// Manage registered game installations
    #[command(subcommand)]
    Game(GameCmd),
    /// Install a mod from a local archive (.zip, .7z); FOMOD installers
    /// are detected automatically and run interactively
    Install {
        /// Path to the mod archive
        archive: PathBuf,
        /// Mod name (default: derived from the archive filename)
        #[arg(long)]
        name: Option<String>,
        /// Mod version
        #[arg(long)]
        version: Option<String>,
        /// Skip the FOMOD installer and install the archive as-is
        #[arg(long)]
        manual: bool,
    },
    /// List installed mods in load order
    Mods,
    /// Enable mods in the active profile
    Enable {
        /// Mod ids or names
        #[arg(required = true)]
        mods: Vec<String>,
    },
    /// Disable mods in the active profile
    Disable {
        #[arg(required = true)]
        mods: Vec<String>,
    },
    /// Move a mod to a position in the load order (1 = loses conflicts, highest = wins)
    Order {
        r#mod: String,
        /// New 1-based position
        position: i64,
    },
    /// Remove an installed mod entirely (all profiles)
    Uninstall { r#mod: String },
    /// Show file conflicts between enabled mods
    Conflicts,
    /// Copy enabled mods into the game directory
    Deploy {
        /// Show the plan without changing any files
        #[arg(long)]
        dry_run: bool,
        /// Overwrite files that were modified outside lmm
        #[arg(long)]
        force: bool,
    },
    /// Remove all deployed files and restore backups
    Purge {
        #[arg(long)]
        dry_run: bool,
        /// Also remove deployed files that were modified outside lmm
        #[arg(long)]
        force: bool,
    },
    /// Check deployed files and staging against recorded state
    Verify,
    /// Fix drift found by verify (redeploy from staging, restore backups)
    Repair {
        #[arg(long)]
        dry_run: bool,
        /// Also overwrite files modified outside lmm
        #[arg(long)]
        force: bool,
    },
    /// Undo an interrupted or failed deployment
    Rollback,
    /// Manage mod profiles
    #[command(subcommand)]
    Profile(ProfileCmd),
    /// Show current installation, profile and deployment state
    Status,
    /// Launch the game (via Steam for Steam installations)
    Launch,
    /// Set the default installation (shorthand for 'game use')
    Use { install: String },
    /// Handle an nxm:// link from Nexus Mods ("Mod Manager Download").
    /// This is what the registered browser handler invokes; it can also be
    /// used manually with a copied link.
    Nxm {
        /// The nxm://... URL
        url: String,
    },
    /// Nexus Mods account and nxm:// handler setup
    #[command(subcommand)]
    Nexus(NexusCmd),
    /// Show and manage Nexus downloads (plain 'downloads' lists them)
    Downloads {
        #[command(subcommand)]
        cmd: Option<DownloadsCmd>,
    },
    /// Inspect, validate and reconfigure FOMOD installers
    #[command(subcommand)]
    Fomod(FomodCmd),
    /// Game Tools: essential modding utilities, one-time setup and
    /// maintenance for the current game (plain 'tools' lists them)
    Tools {
        #[command(subcommand)]
        cmd: Option<ToolsCmd>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ToolsCmd {
    /// Show the game's essential tools and their status (default)
    List,
    /// Install or update a tool from a downloaded archive; without an
    /// archive, shows where to download it
    #[command(alias = "update")]
    Install {
        /// Tool id from 'tools' (e.g. skse, loot)
        tool: String,
        /// Path to the tool's .zip/.7z archive
        archive: Option<PathBuf>,
        /// Version being installed (default: guessed from the filename)
        #[arg(long)]
        version: Option<String>,
        /// Overwrite files that were modified outside lmm
        #[arg(long)]
        force: bool,
    },
    /// Re-check a managed tool's files against the recorded manifest
    Verify {
        /// Tool id (default: every managed tool)
        tool: Option<String>,
    },
    /// Launch a tool (Windows tools run through the game's Proton prefix)
    Launch { tool: String },
    /// Remove a managed tool and restore any files it displaced
    Remove {
        tool: String,
        /// Also remove files that were modified outside lmm
        #[arg(long)]
        force: bool,
    },
    /// Guided first-time setup: essential tools, configuration, load order
    Setup,
    /// Check that the game is ready for modding (a simple checklist)
    Check,
    /// Game settings required for modding (plain 'tools config' shows them)
    Config {
        #[command(subcommand)]
        cmd: Option<ToolsConfigCmd>,
    },
    /// Plugin load order: analyze (default), sort, or restore a backup
    Loadorder {
        #[command(subcommand)]
        cmd: Option<LoadorderCmd>,
    },
}

#[derive(Subcommand, Debug)]
pub enum ToolsConfigCmd {
    /// Show each required setting and whether it is applied (default)
    Show,
    /// Apply the recommended settings (originals are backed up first)
    Apply,
    /// Put every configuration file back to its pre-lmm state
    Restore,
}

#[derive(Subcommand, Debug)]
pub enum LoadorderCmd {
    /// Read plugins.txt and report problems (default)
    Analyze,
    /// Sort plugins with best practices (masters first, dependencies
    /// respected, ties keep their current order); backs up first
    Sort {
        /// Show the resulting order without writing it
        #[arg(long)]
        dry_run: bool,
    },
    /// List automatic load-order backups
    Backups,
    /// Restore the newest load-order backup (or a specific file)
    Restore {
        /// Backup file from 'tools loadorder backups' (default: newest)
        backup: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
pub enum FomodCmd {
    /// Show an installer's steps, groups and options (archive path), or a
    /// mod's saved choices (mod id/name)
    Inspect {
        /// Archive path, or an installed mod's id/name
        target: String,
    },
    /// Show the choices a FOMOD-installed mod was installed with
    Choices { r#mod: String },
    /// Re-run the installer with the saved choices preselected, then
    /// replace the installation after showing the differences
    Reconfigure {
        r#mod: String,
        /// Archive to reinstall from (default: found via the download store)
        #[arg(long)]
        archive: Option<PathBuf>,
    },
    /// Reinstall a mod by replaying its saved plan against the original
    /// archive, without re-asking anything
    Reinstall {
        r#mod: String,
        /// Archive to reinstall from (default: found via the download store)
        #[arg(long)]
        archive: Option<PathBuf>,
    },
    /// Parse an installer and report problems without installing anything
    Validate {
        /// Path to the mod archive
        archive: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
pub enum NexusCmd {
    /// Store your personal Nexus API key (prompted for, never taken as an
    /// argument so it stays out of shell history)
    Apikey,
    /// Forget the stored API key
    Logout,
    /// Register lmm as the system handler for nxm:// links
    Register,
    /// Remove the nxm:// handler registration
    Unregister,
    /// Show API key, account and handler status
    Status,
}

#[derive(Subcommand, Debug)]
pub enum DownloadsCmd {
    /// List downloads (default)
    List,
    /// Start pending downloads. In the shell they run in the background;
    /// as a one-shot command the download runs in the foreground.
    Start {
        /// Download ids from 'downloads'
        #[arg(required_unless_present = "all")]
        ids: Vec<i64>,
        /// Start every pending download
        #[arg(long, conflicts_with = "ids")]
        all: bool,
    },
    /// Cancel a pending or active download
    Cancel { id: i64 },
    /// Re-queue a failed download (needs a fresh nxm link if the old one expired)
    Retry { id: i64 },
    /// Remove a completed/failed download record (the archive stays on disk)
    Remove { id: i64 },
    /// Install a completed download through the normal install pipeline
    Install {
        id: i64,
        /// Override the mod name (default: name from Nexus)
        #[arg(long)]
        name: Option<String>,
    },
}

#[derive(Subcommand, Debug)]
pub enum GameCmd {
    /// Register a game installation manually or from scan results
    Add {
        /// Game root directory
        path: Option<PathBuf>,
        /// Steam app id from 'lmm scan' (alternative to a path)
        #[arg(long, conflicts_with = "path")]
        app: Option<u32>,
        /// Game type slug (see 'lmm scan'; use 'generic' for unknown games)
        #[arg(long)]
        slug: Option<String>,
        /// Human-friendly label to select this installation by
        #[arg(long)]
        label: Option<String>,
    },
    /// List registered installations
    List,
    /// Set the default installation for commands
    Use { install: String },
    /// Unregister an installation (mods must be purged first)
    Remove { install: String },
}

#[derive(Subcommand, Debug)]
pub enum ProfileCmd {
    /// List profiles of the current installation
    List,
    /// Create a new empty profile
    Create { name: String },
    /// Switch the active profile (deploy afterwards to apply)
    Switch { name: String },
    /// Delete a profile
    Delete { name: String },
    /// Duplicate a profile, including enabled state and load order
    Copy { from: String, to: String },
}

// ---------------------------------------------------------------------------
// Completion metadata.
//
// The interactive shell derives command and subcommand names (and flags)
// directly from the clap definitions above, so those can never drift. What
// clap cannot express is what a *positional argument* means — that
// `enable <mod>` wants an installed-but-disabled mod name while
// `profile switch <name>` wants a profile. That mapping lives here, next to
// the commands it describes, as one declarative function instead of string
// checks scattered through the completion engine.

/// What kind of value a positional argument completes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    /// Installation selector: game slug, label, or numeric id.
    Game,
    /// Profile name of the current installation.
    Profile,
    /// Any installed mod of the current installation.
    InstalledMod,
    /// Installed mod currently disabled in the active profile (for `enable`).
    DisabledMod,
    /// Installed mod currently enabled in the active profile (for `disable`).
    EnabledMod,
    /// Download id, narrowed to the statuses the command accepts.
    Download(DownloadFilter),
    /// Tool id from the current game's Game Tools catalog.
    Tool,
    /// Filesystem path.
    Path,
    /// Free text (names being created, URLs, numbers): nothing to suggest.
    None,
}

/// Which download rows a `downloads <sub> <id>` argument may name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadFilter {
    /// `start`: pending (or failed — restartable) rows.
    Startable,
    /// `cancel`: pending or active rows.
    Cancelable,
    /// `retry`: failed rows.
    Failed,
    /// `remove`: completed or failed rows.
    Finished,
    /// `install`: completed rows.
    Completed,
}

/// The completion kind of the `index`-th positional argument (0-based,
/// flags excluded) of the command at `path` (e.g. `["profile", "copy"]`).
///
/// Commands that repeat an argument (`enable a b c`) map every index to the
/// same kind. Anything not listed completes nothing.
pub fn positional_kind(path: &[&str], index: usize) -> CompletionKind {
    use CompletionKind::*;
    match (path, index) {
        (["enable"], _) => DisabledMod,
        (["disable"], _) => EnabledMod,
        (["uninstall"], 0) => InstalledMod,
        (["order"], 0) => InstalledMod,
        (["install"], 0) => Path,
        (["game", "add"], 0) => Path,
        (["use"], 0) | (["game", "use"], 0) | (["game", "remove"], 0) => Game,
        (["profile", "switch"], 0) | (["profile", "delete"], 0) => Profile,
        // `copy <from> <to>`: the source exists, the target is a new name.
        (["profile", "copy"], 0) => Profile,
        (["downloads", "start"], _) => Download(DownloadFilter::Startable),
        (["downloads", "cancel"], 0) => Download(DownloadFilter::Cancelable),
        (["downloads", "retry"], 0) => Download(DownloadFilter::Failed),
        (["downloads", "remove"], 0) => Download(DownloadFilter::Finished),
        (["downloads", "install"], 0) => Download(DownloadFilter::Completed),
        // `fomod inspect` accepts both, but a path is the more likely start.
        (["fomod", "inspect"], 0) => Path,
        (["fomod", "validate"], 0) => Path,
        (["fomod", "choices" | "reconfigure" | "reinstall"], 0) => InstalledMod,
        (["tools", "install" | "update"], 0) => Tool,
        (["tools", "install" | "update"], 1) => Path,
        (["tools", "verify" | "launch" | "remove"], 0) => Tool,
        (["tools", "loadorder", "restore"], 0) => Path,
        _ => None,
    }
}
