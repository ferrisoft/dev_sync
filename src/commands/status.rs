//! `status` (§9.4): what push and pull would do, and what exists only on this machine. Read-only, no network, no
//! lock.

use std::collections::BTreeSet;
use std::path::Path;

use crate::commands::session;
use crate::domain;
use crate::git;
use crate::layout;
use crate::parallel;
use crate::reconcile;
use crate::record;
use crate::report;
use crate::safety;
use crate::scan;
use crate::shell;
use crate::state;
use crate::workspace;


// ==============
// === status ===
// ==============

pub(crate) fn status(context: &session::Context, report: &mut report::Report) -> anyhow::Result<()> {
    let git = &context.git;
    let workspace = workspace::Workspace::discover(git, context.root.as_deref())?;
    let root = workspace.root();
    workspace_status(git, workspace.repository(), report)?;
    let snapshot = workspace::snapshot(git, workspace.repository(), "HEAD")?;
    let base = state::load(&workspace.state_file())?;
    match scan::scan(git, root) {
        Err(error) => report.failure(report::Scope::Disk, format!("{error:#}")),
        Ok(scanned) => {
            leftovers_status(git, &scanned.leftovers, &base, report);
            layout_status(&snapshot, &base, &scanned.repos, report);
            pending_status(root, &snapshot, &base, &scanned.repos, report);
            repos_status(git, root, &snapshot, &base, &scanned.repos, report)?;
        }
    }
    blocked_status(git, root, &base, report);
    if report.is_empty() {
        report.done(report::Scope::Workspace, "nothing to do (as of the last fetch)".to_owned());
    }
    Ok(())
}

/// While a layout merge is in progress, only its own advice is given: pulling and discarding `repos.toml` both fail
/// then.
fn workspace_status(
    git: &git::Git,
    repository: &workspace::Repository,
    report: &mut report::Report,
) -> anyhow::Result<()> {
    let scope = report::Scope::Workspace;
    let repository_word = repository.shell_word();
    let merging = workspace::merge_in_progress(git, repository)?;
    match workspace::current_branch(git, repository)? {
        None => report.attention(
            scope.clone(),
            format!("the workspace HEAD is detached — check out its branch (`git -C {repository_word} switch main`)"),
        ),
        Some(branch) => match workspace::upstream(git, repository, &branch)? {
            None if workspace::has_remote(git, repository, "origin")? => {
                let message = "the workspace isn't published yet — `dev_sync push` publishes it".to_owned();
                report.info(scope.clone(), message);
            }
            None => report.info(
                scope.clone(),
                format!(
                    "the workspace has no origin remote yet — add one with `git -C {repository_word} remote add \
                     origin <url>`"
                ),
            ),
            Some(_) if merging => {}
            Some(upstream) => {
                let name = upstream.short_name();
                match upstream.track {
                    git::Track::InSync => {}
                    git::Track::Ahead(count) => report.info(
                        scope.clone(),
                        format!(
                            "{} not pushed yet — `dev_sync push` publishes them",
                            report::plural(count, "layout commit")
                        ),
                    ),
                    git::Track::Behind(count) => report.info(
                        scope.clone(),
                        format!(
                            "{name} has {} you haven't pulled — `dev_sync pull` merges them",
                            report::plural(count, "layout commit")
                        ),
                    ),
                    git::Track::Diverged { ahead, behind } => report.info(
                        scope.clone(),
                        format!(
                            "the layout is {ahead} ahead and {behind} behind {name} — `dev_sync pull`, then \
                             `dev_sync push`"
                        ),
                    ),
                    git::Track::Gone => {
                        report.attention(scope.clone(), format!("the workspace's upstream {name} no longer exists"));
                    }
                }
            }
        },
    }
    if merging {
        report.attention(
            scope.clone(),
            format!(
                "a layout merge is in progress — resolve {}, then run `dev_sync pull --continue` (or `dev_sync pull \
                 --abort`)",
                repository.layout_file().display()
            ),
        );
    } else if workspace::layout_modified(git, repository)? {
        report.attention(
            scope,
            format!(
                "repos.toml has uncommitted edits — commit or discard them \
                 (`git -C {repository_word} checkout -- repos.toml`)"
            ),
        );
    }
    Ok(())
}

fn leftovers_status(
    git: &git::Git,
    leftovers: &[scan::Leftover],
    base: &state::MachineState,
    report: &mut report::Report,
) {
    for leftover in leftovers {
        match leftover {
            scan::Leftover::Cloning(path) => report.info(
                report::Scope::Disk,
                format!("{} is an unfinished clone from an interrupted run; the next pull removes it", path.display()),
            ),
            scan::Leftover::Moving(path) => {
                let id = domain::FileId::of(&path.join(".git")).ok();
                let owner = id.and_then(|id| base.repos.iter().find(|(_, known)| known.id == id)).map(|(repo, _)| repo);
                let message = match owner {
                    Some(repo) => format!(
                        "{} holds {repo}, parked by an interrupted move; the next pull or push puts it back",
                        path.display()
                    ),
                    None => reconcile::describe_unknown_parked(git, path),
                };
                report.attention(report::Scope::Disk, message);
            }
        }
    }
}

fn layout_status(
    snapshot: &layout::Layout,
    base: &state::MachineState,
    observed: &[scan::ObservedRepo],
    report: &mut report::Report,
) {
    match record::detect(base, observed) {
        Err(error) => report.attention(report::Scope::Layout, format!("{error:#}")),
        Ok(local) => {
            for path in &local.local_only {
                let message = format!("{path} has no origin remote — it exists only on this machine");
                report.info(report::Scope::Layout, message);
            }
            match record::apply_to_snapshot(snapshot, &local.changes) {
                record::Recorded::Unchanged => {}
                record::Recorded::Changed { applied, .. } => {
                    for change in applied {
                        let message = format!("not recorded yet: {change} — `dev_sync push` records it");
                        report.info(report::Scope::Layout, message);
                    }
                }
                record::Recorded::Conflicts(conflicts) => {
                    for conflict in conflicts {
                        report.attention(report::Scope::Layout, conflict.message);
                    }
                }
            }
        }
    }
}

/// The layout's repositories this machine doesn't have yet, and whether a pull can clone them.
fn pending_status(
    root: &Path,
    snapshot: &layout::Layout,
    base: &state::MachineState,
    observed: &[scan::ObservedRepo],
    report: &mut report::Report,
) {
    let on_disk = |repo: &layout::LayoutRepo| {
        observed.iter().any(|seen| seen.path == repo.path && seen.origin == git::Origin::Url(repo.url.clone()))
    };
    let pending = snapshot.repos().filter(|repo| !on_disk(repo) && !base.repos.contains_key(&repo.path));
    let pending = pending.collect::<Vec<_>>();
    let paths = pending.iter().map(|repo| repo.path.clone()).collect::<Vec<_>>();
    match reconcile::collect_facts(root, paths.iter()) {
        Err(error) => report.failure(report::Scope::Layout, format!("{error:#}")),
        Ok(facts) => {
            let landing = reconcile::landing(Vec::new(), pending, &BTreeSet::new(), &BTreeSet::new(), &facts);
            for repo in landing.clones {
                let message = format!("{} isn't on this machine yet — `dev_sync pull` clones it", repo.path);
                report.info(report::Scope::Layout, message);
            }
            for conflict in landing.conflicts {
                let message = format!(
                    "{} isn't on this machine yet, and pull can't put it there: {}",
                    conflict.path,
                    conflict.blocker
                );
                report.attention(report::Scope::Layout, message);
            }
        }
    }
}

fn blocked_status(git: &git::Git, root: &Path, base: &state::MachineState, report: &mut report::Report) {
    for path in base.blocked() {
        let dir = path.to_fs_path(root);
        if std::fs::symlink_metadata(&dir).is_ok() {
            match safety::removal_safety(git, &dir) {
                Err(error) => report.failure(
                    report::Scope::Layout,
                    format!("{path} was removed from the layout, but checking it failed: {error:#}"),
                ),
                Ok(safety::RemovalSafety::Safe) => report.info(
                    report::Scope::Layout,
                    format!("{path} was removed from the layout; the next pull moves it to the Trash"),
                ),
                Ok(safety::RemovalSafety::Unsafe(reasons)) => {
                    let reasons = reasons.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                    report.attention(
                        report::Scope::Layout,
                        format!(
                            "{path} was removed from the layout, but it holds work that exists only here: {reasons} — \
                             push or discard the work, then run `dev_sync pull`; or run `dev_sync keep {}`",
                            shell::word(path.as_str())
                        ),
                    );
                }
            }
        }
    }
}

fn repos_status(
    git: &git::Git,
    root: &Path,
    snapshot: &layout::Layout,
    base: &state::MachineState,
    observed: &[scan::ObservedRepo],
    report: &mut report::Report,
) -> anyhow::Result<()> {
    let tracked = observed
        .iter()
        .filter(|repo| {
            let in_layout =
                snapshot.get(&repo.path).is_some_and(|entry| repo.origin == git::Origin::Url(entry.url.clone()));
            in_layout || base.repos.contains_key(&repo.path)
        })
        .map(|repo| repo.path.clone())
        .collect::<Vec<_>>();
    let inspected = parallel::map(&tracked, git.policy().parallelism, |path| {
        git::inspect(git, &path.to_fs_path(root))
    })?;
    for (path, status) in tracked.iter().zip(inspected) {
        let scope = report::Scope::Repo(path.clone());
        match status {
            Err(error) => report.failure(scope, format!("{error:#}")),
            Ok(status) => describe_repo(path, &status, scope, report),
        }
    }
    Ok(())
}

fn describe_repo(path: &domain::RepoPath, status: &git::RepoStatus, scope: report::Scope, report: &mut report::Report) {
    let tree = status.working_tree;
    if let Some(operation) = status.operation {
        report.attention(scope.clone(), format!("operation in progress ({operation}) — finish or abort it with git"));
    }
    let notes = [
        (tree.tracked_changes || tree.unmerged).then_some("uncommitted changes"),
        tree.untracked.then_some("untracked files"),
        status.has_stash.then_some("stashed changes"),
        matches!(status.head, git::Head::Detached(_)).then_some("detached HEAD"),
    ];
    for note in notes.into_iter().flatten() {
        report.info(scope.clone(), note.to_owned());
    }
    if let git::Head::Unborn(branch) = &status.head {
        report.info(scope.clone(), format!("{branch} has no commits yet"));
    }
    let current = status.current_branch().map(|branch| &branch.name);
    for branch in &status.branches {
        let name = &branch.name;
        match &branch.upstream {
            None => report.info(scope.clone(), format!("{name} has no upstream")),
            Some(upstream) => {
                let target = upstream.short_name();
                match upstream.track {
                    git::Track::InSync => {}
                    git::Track::Ahead(count) => report.info(
                        scope.clone(),
                        format!(
                            "{name} is {} ahead of {target} — `dev_sync push` pushes it",
                            report::plural(count, "commit")
                        ),
                    ),
                    git::Track::Behind(count) => {
                        let hint = match current == Some(name) {
                            true => " — `dev_sync pull` fast-forwards it",
                            false => "",
                        };
                        let commits = report::plural(count, "commit");
                        report.info(scope.clone(), format!("{name} is {commits} behind {target}{hint}"));
                    }
                    git::Track::Diverged { ahead, behind } => report.attention(
                        scope.clone(),
                        format!(
                            "{name} diverged from {target} ({ahead} ahead, {behind} behind) — \
                             resolve with git in {path}"
                        ),
                    ),
                    git::Track::Gone => {
                        report.attention(scope.clone(), format!("upstream {target} of {name} no longer exists"));
                    }
                }
            }
        }
    }
}
