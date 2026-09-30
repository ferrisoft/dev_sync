//! Keeps a dev folder of independent git clones identical across machines. See `docs/design.md`.

mod cli;
mod commands;
mod content;
mod domain;
#[cfg(test)]
mod fixtures;
mod git;
mod layout;
mod listing;
mod parallel;
mod process;
mod reconcile;
mod record;
mod report;
mod safety;
mod scan;
mod shell;
mod state;
mod workspace;

use std::io::IsTerminal as _;
use std::io::Write as _;
use std::process::ExitCode;

use anyhow::Context as _;
use clap::Parser as _;


// ============
// === main ===
// ============

fn main() -> ExitCode {
    match cli::Cli::try_parse() {
        Ok(cli) => run(cli),
        Err(error) => {
            let code = if error.use_stderr() { 1 } else { 0 };
            error.print().ok();
            ExitCode::from(code)
        }
    }
}

/// Runs the command, prints its report to stdout, and turns the outcome into the exit code (§9.10).
fn run(cli: cli::Cli) -> ExitCode {
    match init_logging(cli.verbose).and_then(|()| git::NetworkPolicy::from_env()) {
        Err(error) => fail(&error),
        Ok(policy) => {
            let context = commands::Context {
                git: git::Git::new(policy),
                host: domain::HostName::detect(),
                root: cli.root,
            };
            match cli.command {
                cli::Command::MergeDriver { base, local, incoming, path } => {
                    tracing::debug!(path = %path.display(), "merge driver");
                    match commands::merge_driver(&context.git, &base, &local, &incoming) {
                        Ok(outcome) => ExitCode::from(outcome.exit_code()),
                        Err(error) => {
                            complain(&error);
                            ExitCode::from(commands::DRIVER_FAILED)
                        }
                    }
                }
                cli::Command::Reporting(command) => {
                    let mut report = report::Report::default();
                    let result = dispatch(&context, command, &mut report);
                    print(&report.render(report::use_color()));
                    match result {
                        Ok(()) => report.exit_code(),
                        Err(error) => fail(&error),
                    }
                }
                cli::Command::List { dir } => match commands::list(&context, dir.as_deref()) {
                    Ok(tree) => {
                        print(&tree);
                        ExitCode::SUCCESS
                    }
                    Err(error) => fail(&error),
                },
            }
        }
    }
}

/// Writes `text` to stdout. A closed stdout isn't worth a panic (say, `dev_sync list | head`).
fn print(text: &str) {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(text.as_bytes()).and_then(|()| stdout.flush()).ok();
}

fn fail(error: &anyhow::Error) -> ExitCode {
    complain(error);
    ExitCode::from(1)
}

fn complain(error: &anyhow::Error) {
    writeln!(std::io::stderr(), "error: {error:#}").ok();
}

fn dispatch(
    context: &commands::Context,
    command: cli::ReportingCommand,
    report: &mut report::Report,
) -> anyhow::Result<()> {
    match command {
        cli::ReportingCommand::Init { dir } => commands::init(context, &dir, report),
        cli::ReportingCommand::Status => commands::status(context, report),
        cli::ReportingCommand::Pull { resume, abort } => {
            let mode = match (resume, abort) {
                (true, _) => commands::PullMode::Continue,
                (false, true) => commands::PullMode::Abort,
                (false, false) => commands::PullMode::Plain,
            };
            commands::pull(context, mode, report)
        }
        cli::ReportingCommand::Push => commands::push(context, report),
        cli::ReportingCommand::Keep { path } => commands::keep(context, &path, report),
    }
}


// ====================
// === init_logging ===
// ====================

/// Diagnostics go to stderr: `warn` by default, `debug` with `--verbose`, and `DEV_SYNC_LOG` (a `tracing` filter)
/// overrides both. A log line that can't be written is dropped: reporting that failure would itself panic on the
/// closed stderr (say, `dev_sync pull -v 2>&1 | head`).
fn init_logging(verbose: bool) -> anyhow::Result<()> {
    let filter = match std::env::var("DEV_SYNC_LOG") {
        Ok(spec) => tracing_subscriber::EnvFilter::try_new(&spec)
            .with_context(|| format!("DEV_SYNC_LOG={spec:?} is not a valid log filter"))?,
        Err(std::env::VarError::NotPresent) => {
            tracing_subscriber::EnvFilter::new(if verbose { "debug" } else { "warn" })
        }
        Err(std::env::VarError::NotUnicode(_)) => anyhow::bail!("DEV_SYNC_LOG is not valid UTF-8"),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .without_time()
        .log_internal_errors(false)
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to set up logging: {error}"))
}
