//! `init [DIR] [--remote URL]` (§9.3, revised in §19): sets a dev folder up as a workspace. With an empty repository it
//! creates the workspace and publishes the clones already in the folder; with a published workspace it joins it and
//! clones what is missing; a workspace without a remote gets connected.

use std::io::BufRead;
use std::io::IsTerminal as _;
use std::io::Write;
use std::path::Path;

use anyhow::Context as _;

use crate::commands::pull;
use crate::commands::push;
use crate::commands::session;
use crate::domain;
use crate::git;
use crate::report;
use crate::workspace;


// ============
// === init ===
// ============

pub(crate) fn init(
    context: &session::Context,
    dir: Option<&Path>,
    remote: Option<domain::RemoteUrl>,
    report: &mut report::Report,
) -> anyhow::Result<()> {
    let git = &context.git;
    let dir = dir.unwrap_or(Path::new("."));
    let dir = std::path::absolute(dir).with_context(|| format!("failed to resolve {}", dir.display()))?;
    let start = examine(git, &dir)?;
    let remote = match remote {
        Some(remote) => remote,
        None => {
            let stdin = std::io::stdin();
            let interactive = stdin.is_terminal();
            ask_remote(&mut stdin.lock(), &mut std::io::stderr(), interactive)?
        }
    };
    let scope = report::Scope::Workspace;
    match start {
        Start::Fresh => match clone_repository(git, &dir, &remote)? {
            Cloned::Empty(repository) => {
                workspace::populate(git, &repository, &remote)?;
                let workspace = workspace::Workspace::discover(git, Some(&dir))?;
                let root = workspace.root().display();
                report.done(scope, format!("created the dev_sync workspace in {root}, connected to {remote}"));
                publish(context, workspace, report)
            }
            Cloned::Workspace(workspace) => {
                workspace::add_missing_readme(git, workspace.repository(), &remote)?;
                let root = workspace.root().display();
                report.done(scope, format!("joined the dev_sync workspace from {remote} in {root}"));
                synchronize(context, workspace, report)
            }
        },
        Start::Unconnected(workspace) => {
            workspace::add_origin(git, workspace.repository(), &remote)?;
            workspace::add_missing_readme(git, workspace.repository(), &remote)?;
            let root = workspace.root().display();
            report.done(scope, format!("connected the dev_sync workspace in {root} to {remote}"));
            synchronize(context, workspace, report)
        }
    }
}

/// How `init` found the folder.
enum Start {
    /// No workspace yet.
    Fresh,
    /// A workspace whose repository has no remote.
    Unconnected(workspace::Workspace),
}

/// Refuses a folder `init` can't set up: one inside another workspace, one already connected, or one whose
/// `.dev_sync` is something else.
fn examine(git: &git::Git, dir: &Path) -> anyhow::Result<Start> {
    let hidden = dir.join(workspace::REPOSITORY_DIR);
    match std::fs::symlink_metadata(&hidden) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match workspace::outer_workspace(dir) {
            Some(outer) => Err(anyhow::anyhow!(
                "{} is inside the dev_sync workspace at {}; workspaces can't be nested",
                dir.display(),
                outer.display()
            )),
            None => Ok(Start::Fresh),
        },
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", hidden.display())),
        Ok(_) if workspace::is_workspace(dir) => {
            let workspace = workspace::Workspace::discover(git, Some(dir))?;
            match git::origin(git, workspace.repository().dir())? {
                git::Origin::Url(url) => Err(anyhow::anyhow!(
                    "{} is already a dev_sync workspace, connected to {url}; use `dev_sync pull` and `dev_sync push`",
                    workspace.root().display()
                )),
                git::Origin::Missing => Ok(Start::Unconnected(workspace)),
            }
        }
        Ok(_) => Err(anyhow::anyhow!(
            "{} exists but isn't a dev_sync workspace; move it out of the way, then run `dev_sync init` again",
            hidden.display()
        )),
    }
}

/// Asks for the workspace repository on `output` when `interactive`, and reads the answer from `input`.
fn ask_remote<R, W>(input: &mut R, output: &mut W, interactive: bool) -> anyhow::Result<domain::RemoteUrl> where
R: BufRead,
W: Write {
    if interactive {
        write!(output, "Workspace repository (an empty one on the first machine, e.g. git@github.com:you/dev.git): ")?;
        output.flush()?;
    }
    let mut line = String::new();
    input.read_line(&mut line).context("failed to read the workspace repository")?;
    let answer = line.trim();
    anyhow::ensure!(!answer.is_empty(), "no workspace repository given; pass it with `dev_sync init --remote <url>`");
    answer.parse()
}


// ==============
// === Cloned ===
// ==============

/// What the workspace repository held when `init` cloned it.
enum Cloned {
    /// Nothing yet: this is the first machine.
    Empty(workspace::Repository),
    /// A published workspace.
    Workspace(workspace::Workspace),
}

/// Clones `remote` into `dir`'s `.dev_sync`. A repository with history must hold a workspace; anything else is removed
/// again, which is safe because `init` just made it.
fn clone_repository(git: &git::Git, dir: &Path, remote: &domain::RemoteUrl) -> anyhow::Result<Cloned> {
    std::fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    let root = dir.canonicalize().with_context(|| format!("failed to resolve {}", dir.display()))?;
    let repository = workspace::Repository::of(&root);
    let target = repository.dir().to_path_buf();
    let outcome = git
        .in_directory(&root)
        .args(["clone", "--origin", "origin", "--", remote.as_str(), workspace::REPOSITORY_DIR])
        .remote_with_retry_prep(git::Prompts::Allowed, || remove_clone(&target))?;
    match outcome {
        git::RemoteOutcome::Failed(failure) => {
            remove_clone(&target)?;
            Err(anyhow::anyhow!("{}", failure.describe(&format!("cloning {remote}"))))
        }
        git::RemoteOutcome::Succeeded(_) if workspace::is_unborn(git, &repository)? => Ok(Cloned::Empty(repository)),
        git::RemoteOutcome::Succeeded(_) if workspace::is_workspace(&root) => {
            Ok(Cloned::Workspace(workspace::Workspace::discover(git, Some(&root))?))
        }
        git::RemoteOutcome::Succeeded(_) => {
            remove_clone(&target)?;
            let layout = workspace::LAYOUT_FILE;
            Err(anyhow::anyhow!("{remote} isn't a dev_sync workspace repository (it has no {layout})"))
        }
    }
}

fn remove_clone(clone: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(clone) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("failed to remove {}", clone.display()))
        }
        Ok(()) | Err(_) => Ok(()),
    }
}


// ===============
// === publish ===
// ===============

/// Records the clones in the dev folder and pushes the layout.
fn publish(
    context: &session::Context,
    workspace: workspace::Workspace,
    report: &mut report::Report,
) -> anyhow::Result<()> {
    let session = session::Session::start(context, workspace)?;
    session.require_no_merge()?;
    session.require_clean_layout(report)?;
    match session.record(report)? {
        session::Recording::Conflicted => Ok(()),
        session::Recording::Done => push::push_layout(&session, report),
    }
}

/// Pulls, then pushes the layout — with what the pull recorded on this machine — unless the pull stopped.
fn synchronize(
    context: &session::Context,
    workspace: workspace::Workspace,
    report: &mut report::Report,
) -> anyhow::Result<()> {
    let repository = workspace.repository().clone();
    pull::plain(context, workspace.clone(), report)?;
    let stopped = workspace::merge_in_progress(&context.git, &repository)? || report.has(report::Severity::Failure);
    match stopped {
        true => Ok(()),
        false => push::push_layout(&session::Session::start(context, workspace)?, report),
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use super::ask_remote;

    #[test]
    fn the_remote_is_asked_for_only_on_a_terminal_and_must_be_given() -> anyhow::Result<()> {
        let mut asked = Vec::new();
        let remote = ask_remote(&mut "  git@github.com:you/dev.git \n".as_bytes(), &mut asked, true)?;
        assert_eq!(remote.as_str(), "git@github.com:you/dev.git");
        assert!(String::from_utf8(asked)?.starts_with("Workspace repository"));
        let mut silent = Vec::new();
        let refused = ask_remote(&mut "\n".as_bytes(), &mut silent, false).err().map(|error| format!("{error:#}"));
        assert!(refused.unwrap_or_default().contains("--remote <url>") && silent.is_empty());
        Ok(())
    }
}
