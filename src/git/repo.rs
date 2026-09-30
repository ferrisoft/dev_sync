//! Typed queries about one repository.

use std::ffi::OsStr;
use std::fmt;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use anyhow::Context as _;

use crate::domain;
use crate::git::failure;
use crate::git::parse;
use crate::git::runner;


// ==============
// === Origin ===
// ==============

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    Url(domain::RemoteUrl),
    Missing,
}

pub(crate) fn origin(git: &runner::Git, repo: &Path) -> anyhow::Result<Origin> {
    let finished = git.at(repo).args(["config", "--get", "remote.origin.url"]).run(runner::Access::Read)?;
    match finished.code {
        Some(0) => {
            let text = String::from_utf8(finished.stdout)
                .with_context(|| format!("the origin URL of {} is not valid UTF-8", repo.display()))?;
            let url = text.strip_suffix('\n').unwrap_or(&text);
            url.parse()
                .map(Origin::Url)
                .with_context(|| format!("{} has an origin URL dev_sync can't use", repo.display()))
        }
        Some(1) => Ok(Origin::Missing),
        _ => Err(anyhow::anyhow!(
            "failed to read the origin of {} ({}): {}",
            repo.display(),
            finished.exit(),
            finished.error_text()
        )),
    }
}


// =================
// === Operation ===
// =================

/// A git operation stopped half-way, waiting for the user.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
        })
    }
}

struct Marker {
    name: &'static str,
    operation: Operation,
}

const MARKERS: [Marker; 6] = [
    Marker { name: "rebase-merge", operation: Operation::Rebase },
    Marker { name: "rebase-apply", operation: Operation::Rebase },
    Marker { name: "MERGE_HEAD", operation: Operation::Merge },
    Marker { name: "CHERRY_PICK_HEAD", operation: Operation::CherryPick },
    Marker { name: "REVERT_HEAD", operation: Operation::Revert },
    Marker { name: "BISECT_LOG", operation: Operation::Bisect },
];

/// The operation in progress, found through the files git keeps in the repository's git directory while it waits.
pub(crate) fn operation(git: &runner::Git, repo: &Path) -> anyhow::Result<Option<Operation>> {
    let args = MARKERS.iter().flat_map(|marker| ["--git-path", marker.name]);
    let output = git.at(repo).arg("rev-parse").args(args).run_ok(runner::Access::Read)?;
    let paths = output.split(|byte| *byte == b'\n').filter(|line| !line.is_empty()).collect::<Vec<_>>();
    anyhow::ensure!(paths.len() == MARKERS.len(), "unexpected `git rev-parse --git-path` output in {}", repo.display());
    let found = MARKERS.iter().zip(paths).find_map(|(marker, path)| {
        let path = repo.join(OsStr::from_bytes(path));
        std::fs::symlink_metadata(path).ok().map(|_| marker.operation)
    });
    Ok(found)
}


// ==================
// === RepoStatus ===
// ==================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RepoStatus {
    pub(crate) head: parse::Head,
    pub(crate) working_tree: parse::WorkingTree,
    pub(crate) operation: Option<Operation>,
    pub(crate) has_stash: bool,
    pub(crate) branches: Vec<parse::BranchInfo>,
}

impl RepoStatus {
    /// The branch HEAD is on, if it is on one that has commits.
    pub(crate) fn current_branch(&self) -> Option<&parse::BranchInfo> {
        match &self.head {
            parse::Head::Branch(name) => self.branches.iter().find(|branch| branch.name == *name),
            parse::Head::Detached(_) | parse::Head::Unborn(_) => None,
        }
    }
}

pub(crate) fn inspect(git: &runner::Git, repo: &Path) -> anyhow::Result<RepoStatus> {
    let status = status(git, repo)?;
    Ok(RepoStatus {
        head: status.head,
        working_tree: status.working_tree,
        operation: operation(git, repo)?,
        has_stash: has_stash(git, repo)?,
        branches: branches(git, repo)?,
    })
}


// ===============
// === Queries ===
// ===============

const BRANCH_FORMAT: &str = "--format=%(refname)%00%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)%00\
                             %(upstream:track,nobracket)";

pub(crate) fn status(git: &runner::Git, repo: &Path) -> anyhow::Result<parse::Status> {
    let output = git
        .at(repo)
        .args(["status", "--porcelain=v2", "--branch", "-z", "--untracked-files=normal"])
        .run_ok(runner::Access::Read)?;
    parse::parse_status(&output).with_context(|| format!("failed to read the status of {}", repo.display()))
}

pub(crate) fn branches(git: &runner::Git, repo: &Path) -> anyhow::Result<Vec<parse::BranchInfo>> {
    let output = git.at(repo).args(["for-each-ref", BRANCH_FORMAT, "refs/heads"]).run_ok(runner::Access::Read)?;
    parse::parse_branches(&output).with_context(|| format!("failed to read the branches of {}", repo.display()))
}

pub(crate) fn has_stash(git: &runner::Git, repo: &Path) -> anyhow::Result<bool> {
    let finished = git.at(repo).args(["rev-parse", "--verify", "--quiet", "refs/stash"]).run(runner::Access::Read)?;
    match finished.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => Err(anyhow::anyhow!(
            "failed to check {} for stashed changes ({}): {}",
            repo.display(),
            finished.exit(),
            finished.error_text()
        )),
    }
}

/// Commits on local branches or tags that no remote-tracking ref reaches. A tag the remote also has but that sits on
/// no remote branch counts too, which only errs on the side of keeping a clone.
pub(crate) fn unpushed_on_branches(git: &runner::Git, repo: &Path) -> anyhow::Result<u32> {
    count(git, repo, &["rev-list", "--count", "--branches", "--tags", "--not", "--remotes"])
}

/// Commits reachable from HEAD that no remote-tracking ref reaches.
pub(crate) fn unpushed_from_head(git: &runner::Git, repo: &Path) -> anyhow::Result<u32> {
    count(git, repo, &["rev-list", "--count", "HEAD", "--not", "--remotes"])
}

fn count(git: &runner::Git, repo: &Path, args: &[&str]) -> anyhow::Result<u32> {
    let output = git.at(repo).args(args).run_ok(runner::Access::Read)?;
    let text = String::from_utf8_lossy(&output);
    text.trim().parse().with_context(|| format!("unexpected commit count {:?} from {}", text.trim(), repo.display()))
}


// ===============
// === History ===
// ===============

/// A scratch ref for checking another repository's history against a clone's.
const PROBE: &str = "refs/dev_sync/probe";

/// How a clone relates to another URL's repository.
#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum History {
    /// They share commits (or the clone has none): the same repository, perhaps moved.
    Shared,
    Unrelated,
    Unreachable(failure::RemoteFailure),
}

/// Fetches the default branch of `url` into a scratch ref (removed again) and checks whether any of the clone's
/// commits is also in it.
pub(crate) fn probe_history(git: &runner::Git, repo: &Path, url: &domain::RemoteUrl) -> anyhow::Result<History> {
    let refspec = format!("+HEAD:{PROBE}");
    let fetched = git
        .at(repo)
        .args(["fetch", "--no-tags", "--", url.as_str(), &refspec])
        .remote(runner::Prompts::Allowed)?;
    match fetched {
        failure::RemoteOutcome::Failed(failure) => Ok(History::Unreachable(failure)),
        failure::RemoteOutcome::Succeeded(_) => {
            let total = count(git, repo, &["rev-list", "--count", "--exclude=refs/dev_sync/*", "--all"]);
            let outside_args = ["rev-list", "--count", "--exclude=refs/dev_sync/*", "--all", "--not", PROBE];
            let outside = count(git, repo, &outside_args);
            git.at(repo).args(["update-ref", "-d", PROBE]).run_ok(runner::Access::Write)?;
            let total = total?;
            Ok(match total == 0 || outside? < total {
                true => History::Shared,
                false => History::Unrelated,
            })
        }
    }
}

/// Every worktree, the main one first.
pub(crate) fn worktrees(git: &runner::Git, repo: &Path) -> anyhow::Result<Vec<parse::WorktreeInfo>> {
    let output = git.at(repo).args(["worktree", "list", "--porcelain", "-z"]).run_ok(runner::Access::Read)?;
    parse::parse_worktrees(&output).with_context(|| format!("failed to list the worktrees of {}", repo.display()))
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use crate::fixtures;
    use crate::git::parse;
    use super::Operation;
    use super::Origin;
    use super::has_stash;
    use super::inspect;
    use super::operation;
    use super::origin;
    use super::status;
    use super::unpushed_from_head;
    use super::unpushed_on_branches;
    use super::worktrees;

    #[test]
    fn reads_the_origin_url_or_its_absence() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let clone = sandbox.clone(&remote, &sandbox.path().join("clone"))?;
        let git = fixtures::git();
        assert_eq!(origin(&git, &clone)?, Origin::Url(remote.to_string_lossy().parse()?));
        let local = sandbox.path().join("local");
        std::fs::create_dir(&local)?;
        sandbox.git(&local, &["init", "--quiet"])?;
        assert_eq!(origin(&git, &local)?, Origin::Missing);
        sandbox.git(&local, &["config", "remote.origin.url", "-oProxyCommand=x"])?;
        let message = origin(&git, &local).err().map(|error| format!("{error:#}")).unwrap_or_default();
        assert!(message.contains("local"), "{message}");
        Ok(())
    }

    #[test]
    fn inspects_head_working_tree_and_branches() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let clone = sandbox.clone(&remote, &sandbox.path().join("clone"))?;
        sandbox.commit(&clone, "a", "1")?;
        sandbox.git(&clone, &["branch", "topic"])?;
        std::fs::write(clone.join("README"), "changed")?;
        let status = inspect(&fixtures::git(), &clone)?;
        assert_eq!(status.head, parse::Head::Branch("main".parse()?));
        assert!(status.working_tree.tracked_changes && !status.working_tree.untracked);
        assert_eq!(status.operation, None);
        assert!(!status.has_stash);
        let main = status.branches.iter().find(|branch| branch.name.as_str() == "main");
        let main_track = main.and_then(|main| main.upstream.as_ref()).map(|upstream| upstream.track);
        assert_eq!(main_track, Some(parse::Track::Ahead(1)));
        let topic = status.branches.iter().find(|branch| branch.name.as_str() == "topic");
        assert_eq!(topic.map(|topic| topic.upstream.is_none()), Some(true));
        Ok(())
    }

    #[test]
    fn detects_a_conflicted_merge_and_a_stash() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let clone = sandbox.clone(&remote, &sandbox.path().join("clone"))?;
        sandbox.git(&clone, &["checkout", "--quiet", "-b", "other"])?;
        sandbox.commit(&clone, "README", "other side\n")?;
        sandbox.git(&clone, &["checkout", "--quiet", "main"])?;
        sandbox.commit(&clone, "README", "main side\n")?;
        let merged = sandbox.git(&clone, &["merge", "--quiet", "other"]);
        assert!(merged.is_err(), "the merge should conflict");
        let git = fixtures::git();
        assert_eq!(operation(&git, &clone)?, Some(Operation::Merge));
        assert!(inspect(&git, &clone)?.working_tree.unmerged);
        sandbox.git(&clone, &["merge", "--abort"])?;
        assert_eq!(operation(&git, &clone)?, None);
        std::fs::write(clone.join("README"), "stashed")?;
        sandbox.git(&clone, &["stash", "--quiet"])?;
        assert!(has_stash(&git, &clone)?);
        Ok(())
    }

    #[test]
    fn counts_unpushed_commits_on_branches_and_a_detached_head() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let clone = sandbox.clone(&remote, &sandbox.path().join("clone"))?;
        let git = fixtures::git();
        assert_eq!(unpushed_on_branches(&git, &clone)?, 0);
        sandbox.commit(&clone, "a", "1")?;
        sandbox.commit(&clone, "b", "2")?;
        assert_eq!(unpushed_on_branches(&git, &clone)?, 2);
        sandbox.git(&clone, &["checkout", "--quiet", "--detach", "origin/main"])?;
        sandbox.commit(&clone, "c", "3")?;
        assert_eq!(unpushed_from_head(&git, &clone)?, 1);
        let head = status(&git, &clone)?.head;
        let detached = sandbox.git(&clone, &["rev-parse", "HEAD"])?.trim().parse()?;
        assert_eq!(head, parse::Head::Detached(detached));
        Ok(())
    }

    #[test]
    fn lists_worktrees() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let clone = sandbox.clone(&remote, &sandbox.path().join("clone"))?;
        let linked = clone.join(".claude").join("worktrees").join("x");
        sandbox.git(&clone, &["worktree", "add", "--quiet", "-b", "wt", &linked.to_string_lossy()])?;
        let listed = worktrees(&fixtures::git(), &clone)?;
        let paths = listed.iter().map(|worktree| worktree.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths, vec![clone.canonicalize()?, linked.canonicalize()?]);
        Ok(())
    }
}
