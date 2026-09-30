//! Syncing each repository's commits through its own remote, in the safe ways only (§8.10): fetch, fast-forward,
//! and push of branches that already have an upstream. Never a force push, never a merge or rebase for the user.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::domain;
use crate::git;
use crate::parallel;
use crate::report;


// ====================
// === PullDecision ===
// ====================

/// What a pull does with the current branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum PullDecision {
    FastForward { behind: u32 },
    BehindDirty { behind: u32 },
    Diverged { ahead: u32, behind: u32 },
    UpstreamGone,
    Nothing,
}

/// Only the current branch, and only when no operation is in progress.
pub(crate) fn pull_decision(status: &git::RepoStatus) -> PullDecision {
    let track = status.current_branch().and_then(|branch| branch.upstream.as_ref()).map(|upstream| upstream.track);
    let dirty = status.working_tree.tracked_changes || status.working_tree.unmerged;
    match (status.operation, track) {
        (Some(_), _) => PullDecision::Nothing,
        (None, Some(git::Track::Behind(behind))) if !dirty => PullDecision::FastForward { behind },
        (None, Some(git::Track::Behind(behind))) => PullDecision::BehindDirty { behind },
        (None, Some(git::Track::Diverged { ahead, behind })) => PullDecision::Diverged { ahead, behind },
        (None, Some(git::Track::Gone)) => PullDecision::UpstreamGone,
        (None, Some(git::Track::InSync | git::Track::Ahead(_)) | None) => PullDecision::Nothing,
    }
}


// ==================
// === RemotePush ===
// ==================

/// One `git push` to one remote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemotePush {
    pub(crate) remote: domain::RemoteName,
    /// `refs/heads/<branch>:<remote ref>`; never a `+` refspec.
    pub(crate) refspecs: Vec<String>,
}


// ============
// === Note ===
// ============

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Note {
    pub(crate) severity: report::Severity,
    pub(crate) message: String,
}


// ================
// === PushPlan ===
// ================

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[must_use]
pub(crate) struct PushPlan {
    /// Sorted by remote.
    pub(crate) pushes: Vec<RemotePush>,
    pub(crate) notes: Vec<Note>,
}

/// Every local branch that is ahead of an upstream on a remote gets pushed there. Branches without an upstream are
/// never published; diverged ones and those whose upstream is gone are left to the user.
pub(crate) fn push_plan(status: &git::RepoStatus) -> PushPlan {
    let note = |severity, message: String| Note { severity, message };
    let mut pushes = BTreeMap::<domain::RemoteName, Vec<String>>::new();
    let mut notes = Vec::new();
    let mut unpublished = Vec::new();
    for branch in &status.branches {
        let name = &branch.name;
        let same_name = |upstream: &git::Upstream| {
            upstream.remote_ref.strip_prefix("refs/heads/") == Some(name.as_str())
        };
        let remote = branch.upstream.as_ref().map(|upstream| (upstream, upstream.remote_name()));
        match remote {
            None => unpublished.push(name.as_str()),
            Some((git::Upstream { remote: git::UpstreamRemote::Unusable(remote), .. }, _)) => notes.push(note(
                report::Severity::Attention,
                format!("{name} tracks the remote {remote:?}, which git would take for an option; not pushed"),
            )),
            Some((_, None)) => {}
            Some((upstream, Some(remote))) => {
                let target = upstream.short_name();
                match upstream.track {
                    git::Track::Ahead(_) if !same_name(upstream) => notes.push(note(
                        report::Severity::Info,
                        format!("{name} tracks {target}, which has another name; not pushed (push it with git)"),
                    )),
                    git::Track::Ahead(_) => pushes
                        .entry(remote.clone())
                        .or_default()
                        .push(format!("refs/heads/{name}:{}", upstream.remote_ref)),
                    git::Track::Diverged { ahead, behind } => notes.push(note(
                        report::Severity::Attention,
                        format!("{name} diverged from {target} ({ahead} ahead, {behind} behind); not pushed"),
                    )),
                    git::Track::Gone => notes.push(note(
                        report::Severity::Attention,
                        format!("upstream {target} of {name} no longer exists; not pushing"),
                    )),
                    git::Track::Behind(_) | git::Track::InSync => {}
                }
            }
        }
    }
    notes.extend(unpublished_note(&unpublished).map(|message| note(report::Severity::Info, message)));
    let staying = [
        status.working_tree.tracked_changes.then_some("uncommitted changes stay on this machine"),
        status.working_tree.untracked.then_some("untracked files stay on this machine"),
        status.has_stash.then_some("stashed changes stay on this machine"),
    ];
    notes.extend(staying.into_iter().flatten().map(|message| note(report::Severity::Info, message.to_owned())));
    if let Some(operation) = status.operation {
        notes.push(note(report::Severity::Info, format!("operation in progress ({operation})")));
    }
    let pushes = pushes.into_iter().map(|(remote, refspecs)| RemotePush { remote, refspecs }).collect();
    PushPlan { pushes, notes }
}

/// One line for all branches without an upstream: a clone of a busy repository can have dozens.
pub(crate) fn unpublished_note(branches: &[&str]) -> Option<String> {
    const SHOWN: usize = 5;
    match branches {
        [] => None,
        [branch] => Some(format!("{branch} has no upstream; not pushed (publish it with `git push -u`)")),
        _ => {
            let hidden = branches.len().saturating_sub(SHOWN);
            let more = if hidden > 0 { format!(" (+{hidden} more)") } else { String::new() };
            let shown = branches.iter().take(SHOWN).copied().collect::<Vec<_>>().join(", ");
            Some(format!(
                "{} branches have no upstream; not pushed: {shown}{more} (publish one with `git push -u`)",
                branches.len()
            ))
        }
    }
}


// ================
// === Checkout ===
// ================

/// A repository in the workspace whose contents get synced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Checkout {
    pub(crate) scope: report::Scope,
    pub(crate) dir: PathBuf,
    /// How hints name it, e.g. `ferrisoft/website`.
    pub(crate) label: String,
}

/// Fetches every checkout, then fast-forwards the ones whose current branch is behind and clean.
pub(crate) fn pull(git: &git::Git, checkouts: &[Checkout]) -> anyhow::Result<report::Report> {
    let reports = parallel::map(checkouts, git.policy().parallelism, |checkout| pull_one(git, checkout))?;
    Ok(merged(reports))
}

/// Fetches every checkout, then pushes the branches that are ahead of their upstream.
pub(crate) fn push(git: &git::Git, checkouts: &[Checkout]) -> anyhow::Result<report::Report> {
    let reports = parallel::map(checkouts, git.policy().parallelism, |checkout| push_one(git, checkout))?;
    Ok(merged(reports))
}

fn merged(reports: Vec<report::Report>) -> report::Report {
    reports.into_iter().fold(report::Report::default(), |mut all, report| {
        all.extend(report);
        all
    })
}

/// Fetches and inspects; `None` when that failed, which is already in the report. A fetch that got through to the
/// remote but failed at something smaller, like a tag it wouldn't clobber, updated everything else, so the
/// repository is still inspected.
fn fetch_and_inspect(git: &git::Git, checkout: &Checkout, report: &mut report::Report) -> Option<git::RepoStatus> {
    let scope = || checkout.scope.clone();
    let fetched = git.at(&checkout.dir).args(["fetch", "--all", "--prune"]).remote(git::Prompts::Forbidden);
    let usable = match fetched {
        Ok(git::RemoteOutcome::Succeeded(_)) => true,
        Ok(git::RemoteOutcome::Failed(failure)) => {
            match failure.kind {
                git::RemoteFailureKind::Rejected => {
                    report.attention(scope(), format!("fetch couldn't update some refs: {}", failure.detail));
                }
                git::RemoteFailureKind::Network
                | git::RemoteFailureKind::Stalled { .. }
                | git::RemoteFailureKind::Auth
                | git::RemoteFailureKind::NotFound
                | git::RemoteFailureKind::Other => report.failure(scope(), failure.describe("fetch")),
            }
            failure.reached_the_remote()
        }
        Err(error) => {
            report.failure(scope(), format!("fetch failed: {error:#}"));
            false
        }
    };
    let inspected = usable.then(|| git::inspect(git, &checkout.dir));
    match inspected {
        Some(Ok(status)) => Some(status),
        Some(Err(error)) => {
            report.failure(scope(), format!("{error:#}"));
            None
        }
        None => None,
    }
}

fn pull_one(git: &git::Git, checkout: &Checkout) -> report::Report {
    let mut report = report::Report::default();
    if let Some(status) = fetch_and_inspect(git, checkout, &mut report) {
        let branch = status.current_branch();
        let name = branch.map(|branch| branch.name.to_string()).unwrap_or_default();
        let upstream = branch.and_then(|branch| branch.upstream.as_ref()).map(|up| up.short_name().to_owned());
        let upstream = upstream.unwrap_or_default();
        let scope = checkout.scope.clone();
        let decision = pull_decision(&status);
        tracing::debug!(repo = %checkout.label, ?decision, "pull decision");
        match decision {
            PullDecision::FastForward { behind } => {
                let merged = git
                    .at(&checkout.dir)
                    .args(["merge", "--ff-only", "--progress", "@{upstream}"])
                    .run(git::Access::Lengthy);
                match merged {
                    Ok(finished) if finished.code == Some(0) => {
                        report.done(scope, format!("fast-forwarded {name} by {}", report::plural(behind, "commit")));
                    }
                    Ok(finished) => {
                        report.failure(scope, format!("failed to fast-forward {name}: {}", finished.error_text()));
                    }
                    Err(error) => report.failure(scope, format!("failed to fast-forward {name}: {error:#}")),
                }
            }
            PullDecision::BehindDirty { behind } => report.attention(
                scope,
                format!(
                    "{name} is behind {upstream} by {} but has uncommitted changes — commit or stash them, then pull \
                     again",
                    report::plural(behind, "commit")
                ),
            ),
            PullDecision::Diverged { ahead, behind } => report.attention(
                scope,
                format!(
                    "{name} diverged from {upstream} ({ahead} ahead, {behind} behind) — resolve with git in {}",
                    checkout.label
                ),
            ),
            PullDecision::UpstreamGone => {
                report.attention(scope, format!("upstream {upstream} of {name} no longer exists"));
            }
            PullDecision::Nothing => {}
        }
    }
    report
}

fn push_one(git: &git::Git, checkout: &Checkout) -> report::Report {
    let mut report = report::Report::default();
    if let Some(status) = fetch_and_inspect(git, checkout, &mut report) {
        let plan = push_plan(&status);
        tracing::debug!(repo = %checkout.label, pushes = ?plan.pushes, "push plan");
        for note in plan.notes {
            report.push(note.severity, checkout.scope.clone(), note.message);
        }
        for push in &plan.pushes {
            push_to(git, checkout, push, &mut report);
        }
        if let git::Head::Detached(_) = status.head {
            match git::unpushed_from_head(git, &checkout.dir) {
                Ok(0) => {}
                Ok(count) => report.info(
                    checkout.scope.clone(),
                    format!("a detached HEAD with {} stays on this machine", report::plural(count, "commit")),
                ),
                Err(error) => report.failure(checkout.scope.clone(), format!("{error:#}")),
            }
        }
    }
    report
}

fn push_to(git: &git::Git, checkout: &Checkout, push: &RemotePush, report: &mut report::Report) {
    let remote = push.remote.as_str();
    let outcome = git
        .at(&checkout.dir)
        .args(["push", "--porcelain", remote])
        .args(&push.refspecs)
        .remote(git::Prompts::Forbidden);
    match outcome {
        Ok(git::RemoteOutcome::Succeeded(finished)) => report_refs(checkout, remote, &finished.stdout, report),
        Ok(git::RemoteOutcome::Failed(failure)) => match git::parse_push(&failure.stdout) {
            Ok(refs) if !refs.is_empty() => report_refs(checkout, remote, &failure.stdout, report),
            Ok(_) | Err(_) => report.failure(checkout.scope.clone(), failure.describe(&format!("push to {remote}"))),
        },
        Err(error) => report.failure(checkout.scope.clone(), format!("push to {remote} failed: {error:#}")),
    }
}

/// Reports every ref from `git push --porcelain` output. A ref git rejected because the remote has new commits needs
/// a pull; one the remote refused (a hook, a protected branch) carries the remote's reason.
fn report_refs(checkout: &Checkout, remote: &str, stdout: &[u8], report: &mut report::Report) {
    let scope = || checkout.scope.clone();
    match git::parse_push(stdout) {
        Err(error) => report.failure(scope(), format!("couldn't read the result of the push to {remote}: {error:#}")),
        Ok(refs) => {
            for pushed in refs {
                let branch = pushed.from.strip_prefix("refs/heads/").unwrap_or(&pushed.from);
                match pushed.flag {
                    git::PushFlag::FastForward | git::PushFlag::New | git::PushFlag::Forced => {
                        report.done(scope(), format!("pushed {branch} to {remote}"));
                    }
                    git::PushFlag::Rejected if pushed.summary.starts_with("[rejected]") => report.attention(
                        scope(),
                        format!("push of {branch} to {remote} was rejected — the remote has new commits; pull first"),
                    ),
                    git::PushFlag::Rejected => report.attention(
                        scope(),
                        format!("push of {branch} to {remote} was refused: {}", pushed.summary),
                    ),
                    git::PushFlag::UpToDate | git::PushFlag::Deleted => {}
                }
            }
        }
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::path::PathBuf;

    use crate::fixtures;
    use crate::git;
    use crate::report;
    use super::Checkout;
    use super::PullDecision;
    use super::RemotePush;
    use super::pull;
    use super::pull_decision;
    use super::push;
    use super::push_plan;
    use super::report_refs;

    /// A branch tracking the same-named branch of `remote`.
    fn tracking(name: &str, remote: &str, track: git::Track) -> anyhow::Result<git::BranchInfo> {
        tracking_other(name, remote, name, track)
    }

    /// A branch tracking `remote_branch` of `remote`.
    fn tracking_other(
        name: &str,
        remote: &str,
        remote_branch: &str,
        track: git::Track,
    ) -> anyhow::Result<git::BranchInfo> {
        let full_ref = format!("refs/remotes/{remote}/{remote_branch}");
        let remote = remote
            .parse()
            .map_or_else(|_| git::UpstreamRemote::Unusable(remote.to_owned()), git::UpstreamRemote::Named);
        let remote_ref = format!("refs/heads/{remote_branch}");
        Ok(git::BranchInfo {
            name: name.parse()?,
            upstream: Some(git::Upstream { full_ref, remote, remote_ref, track }),
        })
    }

    fn untracked(name: &str) -> anyhow::Result<git::BranchInfo> {
        Ok(git::BranchInfo { name: name.parse()?, upstream: None })
    }

    fn on_main(track: git::Track, working_tree: git::WorkingTree) -> anyhow::Result<git::RepoStatus> {
        Ok(git::RepoStatus {
            head: git::Head::Branch("main".parse()?),
            working_tree,
            operation: None,
            has_stash: false,
            branches: vec![tracking("main", "origin", track)?],
        })
    }

    const CLEAN: git::WorkingTree = git::WorkingTree { tracked_changes: false, untracked: false, unmerged: false };
    const DIRTY: git::WorkingTree = git::WorkingTree { tracked_changes: true, untracked: false, unmerged: false };
    const UNTRACKED: git::WorkingTree = git::WorkingTree { tracked_changes: false, untracked: true, unmerged: false };

    #[test]
    fn pull_decisions_follow_the_table() -> anyhow::Result<()> {
        assert_eq!(pull_decision(&on_main(git::Track::Behind(3), CLEAN)?), PullDecision::FastForward { behind: 3 });
        assert_eq!(pull_decision(&on_main(git::Track::Behind(3), UNTRACKED)?), PullDecision::FastForward { behind: 3 });
        assert_eq!(pull_decision(&on_main(git::Track::Behind(3), DIRTY)?), PullDecision::BehindDirty { behind: 3 });
        let unmerged = git::WorkingTree { unmerged: true, ..CLEAN };
        assert_eq!(pull_decision(&on_main(git::Track::Behind(3), unmerged)?), PullDecision::BehindDirty { behind: 3 });
        let diverged = git::Track::Diverged { ahead: 2, behind: 1 };
        assert_eq!(pull_decision(&on_main(diverged, CLEAN)?), PullDecision::Diverged { ahead: 2, behind: 1 });
        assert_eq!(pull_decision(&on_main(git::Track::Gone, CLEAN)?), PullDecision::UpstreamGone);
        for track in [git::Track::InSync, git::Track::Ahead(2)] {
            assert_eq!(pull_decision(&on_main(track, CLEAN)?), PullDecision::Nothing);
        }
        let mid_rebase = git::RepoStatus {
            operation: Some(git::Operation::Rebase),
            ..on_main(git::Track::Behind(1), CLEAN)?
        };
        assert_eq!(pull_decision(&mid_rebase), PullDecision::Nothing);
        let detached = git::RepoStatus {
            head: git::Head::Detached("a3c958aec2859e10e9ab44477a1b0740ec3f753c".parse()?),
            ..on_main(git::Track::Behind(1), CLEAN)?
        };
        assert_eq!(pull_decision(&detached), PullDecision::Nothing);
        let unborn = git::RepoStatus {
            head: git::Head::Unborn("main".parse()?),
            branches: vec![],
            ..on_main(git::Track::Behind(1), CLEAN)?
        };
        assert_eq!(pull_decision(&unborn), PullDecision::Nothing);
        let no_upstream = git::RepoStatus {
            branches: vec![untracked("main")?],
            ..on_main(git::Track::Behind(1), CLEAN)?
        };
        assert_eq!(pull_decision(&no_upstream), PullDecision::Nothing);
        Ok(())
    }

    #[test]
    fn push_plans_follow_the_table() -> anyhow::Result<()> {
        let status = git::RepoStatus {
            branches: vec![
                tracking("ahead", "origin", git::Track::Ahead(2))?,
                tracking("mine", "fork", git::Track::Ahead(1))?,
                tracking("diverged", "origin", git::Track::Diverged { ahead: 1, behind: 1 })?,
                tracking("gone", "origin", git::Track::Gone)?,
                tracking("behind", "origin", git::Track::Behind(1))?,
                tracking("same", "origin", git::Track::InSync)?,
                tracking("odd", "-x", git::Track::Ahead(1))?,
                untracked("local")?,
                git::BranchInfo {
                    name: "tracks-local".parse()?,
                    upstream: Some(git::Upstream {
                        full_ref: "refs/heads/main".to_owned(),
                        remote: git::UpstreamRemote::Local,
                        remote_ref: "refs/heads/main".to_owned(),
                        track: git::Track::Ahead(1),
                    }),
                },
            ],
            ..on_main(git::Track::InSync, CLEAN)?
        };
        let plan = push_plan(&status);
        assert_eq!(plan.pushes, vec![
            RemotePush { remote: "fork".parse()?, refspecs: vec!["refs/heads/mine:refs/heads/mine".to_owned()] },
            RemotePush { remote: "origin".parse()?, refspecs: vec!["refs/heads/ahead:refs/heads/ahead".to_owned()] },
        ]);
        let notes = plan.notes.iter().map(|note| (note.severity, note.message.as_str())).collect::<Vec<_>>();
        let diverged = "diverged diverged from origin/diverged (1 ahead, 1 behind); not pushed";
        assert!(notes.contains(&(report::Severity::Attention, diverged)), "{notes:?}");
        let gone = "upstream origin/gone of gone no longer exists; not pushing";
        assert!(notes.contains(&(report::Severity::Attention, gone)), "{notes:?}");
        let odd = "odd tracks the remote \"-x\", which git would take for an option; not pushed";
        assert!(notes.contains(&(report::Severity::Attention, odd)), "{notes:?}");
        let unpublished = "local has no upstream; not pushed (publish it with `git push -u`)";
        assert!(notes.contains(&(report::Severity::Info, unpublished)), "{notes:?}");
        assert_eq!(notes.len(), 4, "{notes:?}");
        Ok(())
    }

    #[test]
    fn push_plans_never_push_to_an_upstream_with_another_name() -> anyhow::Result<()> {
        let status = git::RepoStatus {
            branches: vec![
                tracking_other("feature", "origin", "main", git::Track::Ahead(1))?,
                tracking("main", "origin", git::Track::Ahead(1))?,
            ],
            ..on_main(git::Track::InSync, CLEAN)?
        };
        let plan = push_plan(&status);
        assert_eq!(plan.pushes, vec![RemotePush {
            remote: "origin".parse()?,
            refspecs: vec!["refs/heads/main:refs/heads/main".to_owned()],
        }]);
        let messages = plan.notes.iter().map(|note| note.message.as_str()).collect::<Vec<_>>();
        assert_eq!(messages, vec!["feature tracks origin/main, which has another name; not pushed (push it with git)"]);
        Ok(())
    }

    #[test]
    fn branches_without_an_upstream_share_one_note() -> anyhow::Result<()> {
        let names = ["a", "b", "c", "d", "e", "f", "g"];
        let branches = names.iter().map(|name| untracked(name)).collect::<anyhow::Result<Vec<_>>>()?;
        let status = git::RepoStatus { branches, ..on_main(git::Track::InSync, CLEAN)? };
        let messages = push_plan(&status).notes.into_iter().map(|note| note.message).collect::<Vec<_>>();
        assert_eq!(messages, vec![
            "7 branches have no upstream; not pushed: a, b, c, d, e (+2 more) (publish one with `git push -u`)"
        ]);
        Ok(())
    }

    #[test]
    fn a_push_rejected_for_new_remote_commits_asks_for_a_pull() -> anyhow::Result<()> {
        let mut report = report::Report::default();
        let stdout = b"To /x.git\n!\trefs/heads/main:refs/heads/main\t[rejected] (fetch first)\nDone\n";
        report_refs(&checkout(Path::new("/x"))?, "origin", stdout, &mut report);
        assert_eq!(
            report.render(false),
            "! r: push of main to origin was rejected — the remote has new commits; pull first\n"
        );
        Ok(())
    }

    #[test]
    fn push_mentions_commits_on_a_detached_head() -> anyhow::Result<()> {
        let pair = pair()?;
        pair.sandbox.git(&pair.mine, &["checkout", "--quiet", "--detach"])?;
        pair.sandbox.commit(&pair.mine, "a", "1")?;
        let report = push(&fixtures::git(), &[checkout(&pair.mine)?])?;
        assert_eq!(report.render(false), "· r: a detached HEAD with 1 commit stays on this machine\n");
        Ok(())
    }

    #[test]
    fn a_refused_push_reports_the_remote_s_reason() -> anyhow::Result<()> {
        let pair = pair()?;
        let hook = pair.remote.join("hooks").join("pre-receive");
        std::fs::write(&hook, "#!/bin/sh\necho 'pushes are closed' >&2\nexit 1\n")?;
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        pair.sandbox.commit(&pair.mine, "a", "1")?;
        let report = push(&fixtures::git(), &[checkout(&pair.mine)?])?;
        assert_eq!(
            report.render(false),
            "! r: push of main to origin was refused: [remote rejected] (pre-receive hook declined)\n"
        );
        Ok(())
    }

    #[test]
    fn push_plans_mention_work_that_stays_behind() -> anyhow::Result<()> {
        let working_tree = git::WorkingTree { tracked_changes: true, untracked: true, unmerged: false };
        let status = git::RepoStatus { has_stash: true, ..on_main(git::Track::InSync, working_tree)? };
        let messages = push_plan(&status).notes.into_iter().map(|note| note.message).collect::<Vec<_>>();
        assert_eq!(messages, vec![
            "uncommitted changes stay on this machine",
            "untracked files stay on this machine",
            "stashed changes stay on this machine",
        ]);
        Ok(())
    }

    struct Pair {
        sandbox: fixtures::Sandbox,
        remote: PathBuf,
        mine: PathBuf,
        theirs: PathBuf,
    }

    fn pair() -> anyhow::Result<Pair> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let mine = sandbox.clone(&remote, &sandbox.path().join("mine"))?;
        let theirs = sandbox.clone(&remote, &sandbox.path().join("theirs"))?;
        Ok(Pair { sandbox, remote, mine, theirs })
    }

    fn checkout(dir: &Path) -> anyhow::Result<Checkout> {
        Ok(Checkout { scope: report::Scope::Repo(fixtures::path("r")?), dir: dir.to_path_buf(), label: "r".to_owned() })
    }

    fn head(sandbox: &fixtures::Sandbox, dir: &Path, rev: &str) -> anyhow::Result<String> {
        Ok(sandbox.git(dir, &["rev-parse", rev])?.trim().to_owned())
    }

    #[test]
    fn pull_fast_forwards_a_clean_branch() -> anyhow::Result<()> {
        let pair = pair()?;
        pair.sandbox.commit(&pair.theirs, "a", "1")?;
        pair.sandbox.commit(&pair.theirs, "b", "2")?;
        pair.sandbox.git(&pair.theirs, &["push", "--quiet"])?;
        let report = pull(&fixtures::git(), &[checkout(&pair.mine)?])?;
        assert_eq!(report.render(false), "✓ r: fast-forwarded main by 2 commits\n");
        assert_eq!(head(&pair.sandbox, &pair.mine, "HEAD")?, head(&pair.sandbox, &pair.theirs, "HEAD")?);
        Ok(())
    }

    #[test]
    fn pull_leaves_diverged_and_dirty_branches_alone() -> anyhow::Result<()> {
        let pair = pair()?;
        pair.sandbox.commit(&pair.theirs, "a", "1")?;
        pair.sandbox.git(&pair.theirs, &["push", "--quiet"])?;
        std::fs::write(pair.mine.join("README"), "dirty")?;
        let before = head(&pair.sandbox, &pair.mine, "HEAD")?;
        let dirty = pull(&fixtures::git(), &[checkout(&pair.mine)?])?;
        assert_eq!(dirty.status(), 2);
        let rendered = dirty.render(false);
        assert!(rendered.contains("behind origin/main by 1 commit but has uncommitted changes"), "{rendered}");
        pair.sandbox.git(&pair.mine, &["checkout", "--quiet", "README"])?;
        pair.sandbox.commit(&pair.mine, "b", "2")?;
        let diverged = pull(&fixtures::git(), &[checkout(&pair.mine)?])?;
        assert_eq!(
            diverged.render(false),
            "! r: main diverged from origin/main (1 ahead, 1 behind) — resolve with git in r\n"
        );
        assert_ne!(head(&pair.sandbox, &pair.mine, "HEAD")?, before);
        assert_eq!(head(&pair.sandbox, &pair.mine, "HEAD~1")?, before);
        Ok(())
    }

    #[test]
    fn push_publishes_branches_that_are_ahead_only() -> anyhow::Result<()> {
        let pair = pair()?;
        pair.sandbox.commit(&pair.mine, "a", "1")?;
        pair.sandbox.git(&pair.mine, &["checkout", "--quiet", "-b", "unpublished"])?;
        pair.sandbox.commit(&pair.mine, "b", "2")?;
        let report = push(&fixtures::git(), &[checkout(&pair.mine)?])?;
        let rendered = report.render(false);
        assert!(rendered.contains("✓ r: pushed main to origin"), "{rendered}");
        assert!(rendered.contains("· r: unpublished has no upstream; not pushed"), "{rendered}");
        assert_eq!(report.status(), 0);
        assert_eq!(head(&pair.sandbox, &pair.remote, "main")?, head(&pair.sandbox, &pair.mine, "main")?);
        assert!(pair.sandbox.git(&pair.remote, &["rev-parse", "--verify", "--quiet", "unpublished"]).is_err());
        Ok(())
    }

    #[test]
    fn a_fetch_that_updated_the_rest_still_fast_forwards() -> anyhow::Result<()> {
        let pair = pair()?;
        pair.sandbox.git(&pair.theirs, &["tag", "nightly"])?;
        pair.sandbox.git(&pair.theirs, &["push", "--quiet", "origin", "nightly"])?;
        pair.sandbox.git(&pair.mine, &["fetch", "--quiet", "--tags"])?;
        pair.sandbox.git(&pair.mine, &["config", "fetch.pruneTags", "true"])?;
        pair.sandbox.commit(&pair.theirs, "a", "1")?;
        pair.sandbox.git(&pair.theirs, &["tag", "--force", "nightly"])?;
        pair.sandbox.git(&pair.theirs, &["push", "--quiet", "origin", "main"])?;
        pair.sandbox.git(&pair.theirs, &["push", "--quiet", "--force", "origin", "nightly"])?;
        let report = pull(&fixtures::git(), &[checkout(&pair.mine)?])?;
        let rendered = report.render(false);
        assert!(rendered.contains("! r: fetch couldn't update some refs: ! [rejected]"), "{rendered}");
        assert!(rendered.contains("(would clobber existing tag)"), "{rendered}");
        assert!(rendered.contains("✓ r: fast-forwarded main by 1 commit"), "{rendered}");
        assert_eq!(report.status(), 2, "{rendered}");
        Ok(())
    }

    #[test]
    fn a_failed_fetch_is_reported_and_skips_the_repo() -> anyhow::Result<()> {
        let pair = pair()?;
        let gone = pair.sandbox.path().join("gone.git");
        pair.sandbox.git(&pair.mine, &["remote", "set-url", "origin", &gone.to_string_lossy()])?;
        let report = pull(&fixtures::git(), &[checkout(&pair.mine)?, checkout(&pair.theirs)?])?;
        assert_eq!(report.status(), 1);
        assert!(report.render(false).contains("✗ r: fetch failed (not found)"), "{}", report.render(false));
        Ok(())
    }
}
