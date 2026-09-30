//! Command-line interface (§9.1).

use std::path::PathBuf;


// ===========
// === Cli ===
// ===========

/// Keeps a dev folder of independent git clones identical across machines.
#[derive(Debug, clap::Parser)]
#[command(name = "dev_sync")]
pub(crate) struct Cli {
    /// The workspace root (default: the nearest directory above holding a .dev_sync folder)
    #[arg(long, global = true, value_name = "DIR")]
    pub(crate) root: Option<PathBuf>,
    /// Log every git command and more to stderr
    #[arg(long, short, global = true)]
    pub(crate) verbose: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}


// ===============
// === Command ===
// ===============

#[derive(Debug, clap::Subcommand)]
pub(crate) enum Command {
    #[command(flatten)]
    Reporting(ReportingCommand),
    /// The git merge driver for repos.toml
    #[command(hide = true)]
    MergeDriver {
        base: PathBuf,
        local: PathBuf,
        incoming: PathBuf,
        path: PathBuf,
    },
}


// ========================
// === ReportingCommand ===
// ========================

/// The commands that end with a report.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum ReportingCommand {
    /// Make DIR a workspace: its layout goes in a git repository in DIR/.dev_sync
    Init {
        /// The dev folder (relative to the current directory); clones already in it stay as they are
        dir: PathBuf,
    },
    /// Show what push and pull would do (no network)
    Status,
    /// Merge layout changes from the remote, apply them, update repos
    Pull {
        /// Finish a pull after resolving repos.toml
        #[arg(long = "continue", conflicts_with = "abort")]
        resume: bool,
        /// Abandon a conflicted pull
        #[arg(long)]
        abort: bool,
    },
    /// Record local layout changes, push repos, push the layout
    Push,
    /// Put a blocked removal back into the layout
    Keep {
        /// The blocked repository (relative to the current directory)
        path: PathBuf,
    },
    /// Add the repos found under DIR to the layout (clones nothing)
    Import {
        /// A tree of clones outside the workspace; it is only read
        dir: PathBuf,
    },
}
