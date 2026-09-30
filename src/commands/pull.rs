//! `pull [--continue | --abort]` (§9.5).

use anyhow::Context as _;

use crate::commands::session;
use crate::content;
use crate::domain;
use crate::git;
use crate::layout;
use crate::report;
use crate::workspace;


// ================
// === PullMode ===
// ================

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PullMode {
    Plain,
    Continue,
    Abort,
}


// ============
// === pull ===
// ============

pub(crate) fn pull(context: &session::Context, mode: PullMode, report: &mut report::Report) -> anyhow::Result<()> {
    let workspace = workspace::Workspace::discover(&context.git, context.root.as_deref())?;
    match mode {
        PullMode::Plain => plain(context, workspace, report),
        PullMode::Continue => resume(context, workspace, report),
        PullMode::Abort => abort(context, workspace, report),
    }
}

/// Record local changes, merge the remote layout, make the disk match, then pull repo contents.
fn plain(
    context: &session::Context,
    workspace: workspace::Workspace,
    report: &mut report::Report,
) -> anyhow::Result<()> {
    let session = session::Session::start(context, workspace)?;
    session.require_no_merge()?;
    session.require_clean_layout(report)?;
    match session.record(report)? {
        session::Recording::Conflicted => {}
        session::Recording::Done => match merge_upstream(&session, report)? {
            Merge::Stopped => {}
            Merge::Done => finish(&session, report)?,
        },
    }
    if report.is_empty() {
        report.done(report::Scope::Workspace, "already up to date".to_owned());
    }
    Ok(())
}

/// Finishes a merge whose conflicts the user resolved in `repos.toml`, then carries on like a pull. The resolution
/// may settle each conflict either way, but must keep everything that merged cleanly: a merge whose driver failed
/// leaves only the local side behind, and committing that would silently drop the incoming changes.
fn resume(
    context: &session::Context,
    workspace: workspace::Workspace,
    report: &mut report::Report,
) -> anyhow::Result<()> {
    let session = session::Session::start(context, workspace)?;
    let (git, repository) = (session.git(), session.repository());
    anyhow::ensure!(workspace::merge_in_progress(git, repository)?, "there is no layout merge to continue");
    let file = repository.layout_file();
    let text = std::fs::read_to_string(&file).with_context(|| format!("failed to read {}", file.display()))?;
    let resolved = layout::parse(&text)?;
    let stray = layout::stray_changes(&predict(git, repository, "MERGE_HEAD")?, &resolved);
    anyhow::ensure!(
        stray.is_empty(),
        "repos.toml changes what merged cleanly ({}); resolve only the conflicts, or start over with `dev_sync pull \
         --abort` and `dev_sync pull`",
        stray.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")
    );
    workspace::write_layout(repository, &resolved)?;
    git.at(repository.dir()).args(["add", "--", workspace::LAYOUT_FILE]).run_ok(git::Access::Write)?;
    let unmerged = workspace::unmerged_paths(git, repository)?;
    anyhow::ensure!(
        unmerged.is_empty(),
        "these files still have conflicts: {}; resolve them with git in {}, then run `dev_sync pull --continue`",
        unmerged.join(", "),
        repository.dir().display()
    );
    git.at(repository.dir()).args(["commit", "--no-edit", "--quiet"]).run_ok(git::Access::Lengthy)?;
    report.done(report::Scope::Workspace, "finished merging the layout".to_owned());
    match session.record(report)? {
        session::Recording::Conflicted => Ok(()),
        session::Recording::Done => finish(&session, report),
    }
}

fn abort(
    context: &session::Context,
    workspace: workspace::Workspace,
    report: &mut report::Report,
) -> anyhow::Result<()> {
    let session = session::Session::start(context, workspace)?;
    let (git, repository) = (session.git(), session.repository());
    anyhow::ensure!(workspace::merge_in_progress(git, repository)?, "there is no layout merge to abort");
    git.at(repository.dir()).args(["merge", "--abort"]).run_ok(git::Access::Write)?;
    report.done(report::Scope::Workspace, "aborted the layout merge".to_owned());
    Ok(())
}

fn finish(session: &session::Session<'_>, report: &mut report::Report) -> anyhow::Result<()> {
    if let Some(base) = session.reconcile(report)? {
        report.extend(content::pull(session.git(), &session.checkouts(&base))?);
    }
    Ok(())
}


// =============
// === Merge ===
// =============

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
enum Merge {
    /// Merged, already up to date, or nothing to merge yet: the disk can follow HEAD.
    Done,
    /// A conflict or a failure, already in the report; the pull stops here.
    Stopped,
}

enum Tracking {
    Found(git::Upstream),
    /// The remote has no branch to merge yet.
    Missing,
    Failed,
}

/// Merges the upstream's layout into HEAD (§9.5 steps 4–7). A branch without an upstream starts tracking
/// `origin/<branch>` once that exists.
fn merge_upstream(session: &session::Session<'_>, report: &mut report::Report) -> anyhow::Result<Merge> {
    let tracking = match workspace::upstream(session.git(), session.repository(), &session.branch)? {
        Some(upstream) => Tracking::Found(upstream),
        None => track_origin(session, report)?,
    };
    match tracking {
        Tracking::Failed => Ok(Merge::Stopped),
        Tracking::Missing => Ok(Merge::Done),
        Tracking::Found(upstream) => fetch_and_merge(session, &upstream, report),
    }
}

fn track_origin(session: &session::Session<'_>, report: &mut report::Report) -> anyhow::Result<Tracking> {
    let (git, repository, branch) = (session.git(), session.repository(), &session.branch);
    anyhow::ensure!(
        workspace::has_remote(git, repository, "origin")?,
        "the workspace repo has no origin remote; add one with `git -C {} remote add origin <url>`",
        repository.shell_word()
    );
    match fetch(session, &domain::RemoteName::origin(), report)? {
        Fetched::Failed => Ok(Tracking::Failed),
        Fetched::Done => match workspace::ref_exists(git, repository, &format!("refs/remotes/origin/{branch}"))? {
            false => {
                let message = format!("origin has no {branch} yet; `dev_sync push` creates it");
                report.info(report::Scope::Workspace, message);
                Ok(Tracking::Missing)
            }
            true => {
                let target = format!("--set-upstream-to=origin/{branch}");
                let tracked = git.at(repository.dir()).args(["branch", "--quiet", &target, branch.as_str()]);
                tracked.run_ok(git::Access::Write)?;
                let upstream = workspace::upstream(git, repository, branch)?;
                Ok(upstream.map_or(Tracking::Missing, Tracking::Found))
            }
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
enum Fetched {
    Done,
    /// The failure is in the report.
    Failed,
}

fn fetch(
    session: &session::Session<'_>,
    remote: &domain::RemoteName,
    report: &mut report::Report,
) -> anyhow::Result<Fetched> {
    let outcome = session
        .git()
        .at(session.repository().dir())
        .args(["fetch", "--prune", remote.as_str()])
        .remote(git::Prompts::Allowed)?;
    Ok(match outcome {
        git::RemoteOutcome::Succeeded(_) => Fetched::Done,
        git::RemoteOutcome::Failed(failure) => {
            report.failure(report::Scope::Workspace, failure.describe(&format!("fetching the layout from {remote}")));
            Fetched::Failed
        }
    })
}

fn fetch_and_merge(
    session: &session::Session<'_>,
    upstream: &git::Upstream,
    report: &mut report::Report,
) -> anyhow::Result<Merge> {
    let (git, repository) = (session.git(), session.repository());
    let fetched = match (&upstream.remote, upstream.remote_name()) {
        (git::UpstreamRemote::Unusable(remote), _) => Err(workspace::unusable_remote(remote, &session.branch)),
        (_, Some(remote)) => fetch(session, remote, report),
        (_, None) => Ok(Fetched::Done),
    }?;
    match fetched {
        Fetched::Failed => Ok(Merge::Stopped),
        Fetched::Done => match workspace::ref_exists(git, repository, &upstream.full_ref)? {
            false => {
                report.info(
                    report::Scope::Workspace,
                    format!("{} doesn't exist yet; `dev_sync push` creates it", upstream.short_name()),
                );
                Ok(Merge::Done)
            }
            true => merge(session, upstream, report),
        },
    }
}

/// Merges the upstream's layout into HEAD. Git runs this very executable as the merge driver, and whenever the merge
/// stops before committing, `repos.toml` gets the in-process merge of the three layouts. So the result never depends
/// on a driver having run (a missing `.gitattributes`, a driver that can't start) or on git's text merge.
fn merge(
    session: &session::Session<'_>,
    upstream: &git::Upstream,
    report: &mut report::Report,
) -> anyhow::Result<Merge> {
    let (git, repository) = (session.git(), session.repository());
    let before = workspace::snapshot(git, repository, "HEAD")?;
    let predicted = predict(git, repository, &upstream.full_ref)?;
    let executable = std::env::current_exe().context("failed to find the dev_sync executable")?;
    let driver = format!("merge.dev-sync.driver={}", workspace::driver_command(&executable)?);
    let merged = git
        .at(repository.dir())
        .args(["-c", &driver, "merge", "--no-edit", "--quiet", "--ff", "--no-commit", &upstream.full_ref])
        .run(git::Access::Lengthy)?;
    let merging = workspace::merge_in_progress(git, repository)?;
    tracing::debug!(
        upstream = %upstream.full_ref,
        merging,
        exit = %merged.exit(),
        predicted = ?predicted,
        "merged the layout"
    );
    match (merging, merged.code) {
        (true, _) => settle(session, &before, predicted, report),
        (false, Some(0)) => {
            report_incoming(&before, &workspace::snapshot(git, repository, "HEAD")?, report);
            Ok(Merge::Done)
        }
        (false, _) => {
            let stderr = String::from_utf8_lossy(&merged.stderr);
            report.failure(
                report::Scope::Workspace,
                format!(
                    "merging the layout from {} failed: {}",
                    upstream.short_name(),
                    stderr.trim().replace('\n', "; ")
                ),
            );
            Ok(Merge::Stopped)
        }
    }
}

/// The in-process merge of HEAD's layout with `other`'s, from their merge base.
fn predict(
    git: &git::Git,
    repository: &workspace::Repository,
    other: &str,
) -> anyhow::Result<layout::MergeOutcome> {
    let local = workspace::snapshot(git, repository, "HEAD")?;
    let incoming = workspace::snapshot(git, repository, other)?;
    let base = match workspace::merge_base(git, repository, other)? {
        Some(commit) => workspace::snapshot(git, repository, &commit.to_string())?,
        None => layout::Layout::default(),
    };
    Ok(layout::merge(&base, &local, &incoming))
}

/// Writes the in-process merge into `repos.toml` and commits the merge when that is clean and no other file
/// conflicts.
fn settle(
    session: &session::Session<'_>,
    before: &layout::Layout,
    predicted: layout::MergeOutcome,
    report: &mut report::Report,
) -> anyhow::Result<Merge> {
    let (git, repository) = (session.git(), session.repository());
    match predicted {
        layout::MergeOutcome::Clean(merged) => {
            workspace::write_layout(repository, &merged)?;
            git.at(repository.dir()).args(["add", "--", workspace::LAYOUT_FILE]).run_ok(git::Access::Write)?;
            let unmerged = workspace::unmerged_paths(git, repository)?;
            match unmerged.is_empty() {
                true => {
                    let committed = git.at(repository.dir()).args(["commit", "--no-edit", "--quiet"]);
                    committed.run_ok(git::Access::Lengthy)?;
                    report_incoming(before, &merged, report);
                    Ok(Merge::Done)
                }
                false => {
                    report_conflicts(repository, &[], &unmerged, report);
                    Ok(Merge::Stopped)
                }
            }
        }
        layout::MergeOutcome::Conflicted(conflicted) => {
            workspace::write_layout_text(repository, &layout::render_conflicted(&conflicted))?;
            let unmerged = workspace::unmerged_paths(git, repository)?;
            report_conflicts(repository, &conflicted.conflicts, &unmerged, report);
            Ok(Merge::Stopped)
        }
    }
}

fn report_incoming(before: &layout::Layout, after: &layout::Layout, report: &mut report::Report) {
    for change in layout::diff(before, after) {
        report.info(report::Scope::Layout, format!("incoming: {change}"));
    }
}

fn report_conflicts(
    repository: &workspace::Repository,
    conflicts: &[layout::Conflict],
    unmerged: &[String],
    report: &mut report::Report,
) {
    for conflict in conflicts {
        report.attention(report::Scope::Layout, format!("conflict: {conflict}"));
    }
    let others = unmerged.iter().filter(|path| *path != workspace::LAYOUT_FILE).collect::<Vec<_>>();
    for path in &others {
        report.attention(report::Scope::Layout, format!("{path} has a merge conflict too — resolve it with git"));
    }
    let (layout_file, dir) = (repository.layout_file(), repository.dir());
    let what = match (conflicts.is_empty(), others.is_empty()) {
        (false, true) => layout_file.display().to_string(),
        (false, false) => format!("{} and those files", layout_file.display()),
        (true, _) => format!("those files in {}", dir.display()),
    };
    report.attention(
        report::Scope::Layout,
        format!("resolve {what}, then run `dev_sync pull --continue` (or `dev_sync pull --abort`)"),
    );
}
