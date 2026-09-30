//! Whether a clone can go to the Trash without losing work that exists only here (§8.9).

use std::fmt;
use std::path::Path;
use std::path::PathBuf;

use crate::git;
use crate::report;


// =====================
// === RemovalSafety ===
// =====================

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum RemovalSafety {
    Safe,
    Unsafe(Vec<UnsafeReason>),
}

/// Collects every reason the clone at `repo` isn't safe to trash, not just the first. Ignored files don't count: the
/// Trash keeps them anyway.
pub(crate) fn removal_safety(git: &git::Git, repo: &Path) -> anyhow::Result<RemovalSafety> {
    let is_repo = std::fs::symlink_metadata(repo.join(".git")).is_ok_and(|metadata| metadata.is_dir());
    let reasons = match is_repo {
        true => reasons(git, repo)?,
        false => vec![UnsafeReason::NotARepository],
    };
    Ok(match reasons.is_empty() {
        true => RemovalSafety::Safe,
        false => RemovalSafety::Unsafe(reasons),
    })
}

fn reasons(git: &git::Git, repo: &Path) -> anyhow::Result<Vec<UnsafeReason>> {
    let status = git::status(git, repo)?;
    let tree = status.working_tree;
    let unpushed = git::unpushed_on_branches(git, repo)?;
    let detached = match status.head {
        git::Head::Detached(_) => git::unpushed_from_head(git, repo)?,
        git::Head::Branch(_) | git::Head::Unborn(_) => 0,
    };
    let linked = git::worktrees(git, repo)?.into_iter().skip(1).filter(|worktree| !worktree.prunable);
    let worktrees = linked.map(|worktree| worktree_problems(git, &worktree)).collect::<anyhow::Result<Vec<_>>>()?;
    let own = [
        (tree.tracked_changes || tree.unmerged).then_some(UnsafeReason::UncommittedChanges),
        tree.untracked.then_some(UnsafeReason::UntrackedFiles),
        git::operation(git, repo)?.map(UnsafeReason::OperationInProgress),
        git::has_stash(git, repo)?.then_some(UnsafeReason::Stash),
        (unpushed > 0).then_some(UnsafeReason::UnpushedCommits { count: unpushed }),
        (detached > 0).then_some(UnsafeReason::DetachedCommits { count: detached }),
    ];
    Ok(own.into_iter().flatten().chain(worktrees.into_iter().flatten()).collect())
}

fn worktree_problems(git: &git::Git, worktree: &git::WorktreeInfo) -> anyhow::Result<Vec<UnsafeReason>> {
    let path = &worktree.path;
    let problems = match std::fs::symlink_metadata(path) {
        Err(_) => vec![WorktreeProblem::Inaccessible],
        Ok(_) => {
            let tree = git::status(git, path)?.working_tree;
            let detached = match worktree.head {
                git::WorktreeHead::Detached(_) => git::unpushed_from_head(git, path)?,
                git::WorktreeHead::Branch(_) | git::WorktreeHead::Bare => 0,
            };
            [
                (tree.tracked_changes || tree.unmerged).then_some(WorktreeProblem::UncommittedChanges),
                tree.untracked.then_some(WorktreeProblem::UntrackedFiles),
                git::operation(git, path)?.map(WorktreeProblem::OperationInProgress),
                (detached > 0).then_some(WorktreeProblem::UnpushedCommits { count: detached }),
            ]
            .into_iter()
            .flatten()
            .collect()
        }
    };
    Ok(problems.into_iter().map(|problem| UnsafeReason::Worktree { path: path.clone(), problem }).collect())
}


// ====================
// === UnsafeReason ===
// ====================

/// Work that exists only in this clone.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UnsafeReason {
    /// The directory lost its `.git`, so nothing about it can be checked.
    NotARepository,
    UncommittedChanges,
    UntrackedFiles,
    OperationInProgress(git::Operation),
    Stash,
    /// Commits on local branches that no remote-tracking ref reaches.
    UnpushedCommits { count: u32 },
    /// Commits reachable only from a detached HEAD.
    DetachedCommits { count: u32 },
    Worktree { path: PathBuf, problem: WorktreeProblem },
}

impl fmt::Display for UnsafeReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotARepository => f.write_str("it has no .git directory"),
            Self::UncommittedChanges => f.write_str("uncommitted changes"),
            Self::UntrackedFiles => f.write_str("untracked files"),
            Self::OperationInProgress(operation) => write!(f, "operation in progress ({operation})"),
            Self::Stash => f.write_str("stashed changes"),
            Self::UnpushedCommits { count } => write!(f, "{} not on any remote", report::plural(*count, "commit")),
            Self::DetachedCommits { count } => {
                write!(f, "a detached HEAD with {} not on any remote", report::plural(*count, "commit"))
            }
            Self::Worktree { path, problem } => write!(f, "worktree {}: {problem}", path.display()),
        }
    }
}


// =======================
// === WorktreeProblem ===
// =======================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorktreeProblem {
    UncommittedChanges,
    UntrackedFiles,
    /// A detached worktree's commits that no remote-tracking ref reaches.
    UnpushedCommits { count: u32 },
    OperationInProgress(git::Operation),
    /// Listed by git, not marked prunable, yet unreadable (e.g. locked on a drive that isn't mounted).
    Inaccessible,
}

impl fmt::Display for WorktreeProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UncommittedChanges => f.write_str("uncommitted changes"),
            Self::UntrackedFiles => f.write_str("untracked files"),
            Self::UnpushedCommits { count } => {
                write!(f, "a detached HEAD with {} not on any remote", report::plural(*count, "commit"))
            }
            Self::OperationInProgress(operation) => write!(f, "operation in progress ({operation})"),
            Self::Inaccessible => f.write_str("can't be read (is it on a drive that isn't mounted?)"),
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
    use super::RemovalSafety;
    use super::UnsafeReason;
    use super::WorktreeProblem;
    use super::removal_safety;

    struct Setup {
        sandbox: fixtures::Sandbox,
        clone: PathBuf,
    }

    fn setup() -> anyhow::Result<Setup> {
        let sandbox = fixtures::Sandbox::create()?;
        let remote = sandbox.remote("r")?;
        let clone = sandbox.clone(&remote, &sandbox.path().join("clone"))?;
        Ok(Setup { sandbox, clone })
    }

    fn reasons(setup: &Setup) -> anyhow::Result<Vec<UnsafeReason>> {
        Ok(match removal_safety(&fixtures::git(), &setup.clone)? {
            RemovalSafety::Safe => vec![],
            RemovalSafety::Unsafe(reasons) => reasons,
        })
    }

    #[test]
    fn a_clean_pushed_clone_is_safe() -> anyhow::Result<()> {
        let setup = setup()?;
        std::fs::write(setup.clone.join(".git").join("info").join("exclude"), "ignored-file\n")?;
        std::fs::write(setup.clone.join("ignored-file"), "ignored files don't count")?;
        assert_eq!(removal_safety(&fixtures::git(), &setup.clone)?, RemovalSafety::Safe);
        Ok(())
    }

    #[test]
    fn uncommitted_changes_are_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        std::fs::write(setup.clone.join("README"), "edited")?;
        assert_eq!(reasons(&setup)?, vec![UnsafeReason::UncommittedChanges]);
        Ok(())
    }

    #[test]
    fn untracked_files_are_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        std::fs::write(setup.clone.join("notes"), "new")?;
        assert_eq!(reasons(&setup)?, vec![UnsafeReason::UntrackedFiles]);
        Ok(())
    }

    #[test]
    fn an_operation_in_progress_is_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        setup.sandbox.git(&setup.clone, &["checkout", "--quiet", "-b", "side"])?;
        setup.sandbox.commit(&setup.clone, "README", "side\n")?;
        setup.sandbox.git(&setup.clone, &["checkout", "--quiet", "main"])?;
        setup.sandbox.commit(&setup.clone, "README", "main\n")?;
        assert!(setup.sandbox.git(&setup.clone, &["merge", "--quiet", "side"]).is_err());
        let found = reasons(&setup)?;
        assert!(found.contains(&UnsafeReason::OperationInProgress(git::Operation::Merge)), "{found:?}");
        assert!(found.contains(&UnsafeReason::UncommittedChanges), "{found:?}");
        assert!(found.iter().any(|reason| reason.to_string().contains("operation in progress")), "{found:?}");
        Ok(())
    }

    #[test]
    fn a_stash_is_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        std::fs::write(setup.clone.join("README"), "stashed")?;
        setup.sandbox.git(&setup.clone, &["stash", "--quiet"])?;
        assert_eq!(reasons(&setup)?, vec![UnsafeReason::Stash]);
        Ok(())
    }

    #[test]
    fn unpushed_commits_are_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        setup.sandbox.commit(&setup.clone, "a", "1")?;
        setup.sandbox.git(&setup.clone, &["checkout", "--quiet", "-b", "local-only"])?;
        setup.sandbox.commit(&setup.clone, "b", "2")?;
        assert_eq!(reasons(&setup)?, vec![UnsafeReason::UnpushedCommits { count: 2 }]);
        Ok(())
    }

    #[test]
    fn a_commit_only_a_tag_holds_is_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        setup.sandbox.git(&setup.clone, &["checkout", "--quiet", "--detach"])?;
        setup.sandbox.commit(&setup.clone, "a", "1")?;
        setup.sandbox.git(&setup.clone, &["tag", "kept"])?;
        setup.sandbox.git(&setup.clone, &["checkout", "--quiet", "main"])?;
        assert_eq!(reasons(&setup)?, vec![UnsafeReason::UnpushedCommits { count: 1 }]);
        Ok(())
    }

    #[test]
    fn commits_on_a_detached_head_are_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        setup.sandbox.git(&setup.clone, &["checkout", "--quiet", "--detach"])?;
        setup.sandbox.commit(&setup.clone, "a", "1")?;
        assert_eq!(reasons(&setup)?, vec![UnsafeReason::DetachedCommits { count: 1 }]);
        Ok(())
    }

    #[test]
    fn a_missing_git_directory_is_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        std::fs::remove_dir_all(setup.clone.join(".git"))?;
        assert_eq!(reasons(&setup)?, vec![UnsafeReason::NotARepository]);
        Ok(())
    }

    #[test]
    fn dirty_or_ahead_linked_worktrees_are_unsafe() -> anyhow::Result<()> {
        let setup = setup()?;
        let dirty = setup.clone.join(".claude").join("worktrees").join("dirty");
        let detached = setup.sandbox.path().join("detached");
        let clean = setup.sandbox.path().join("clean");
        let pruned = setup.sandbox.path().join("pruned");
        let add = |args: &[&str]| setup.sandbox.git(&setup.clone, &[&["worktree", "add", "--quiet"], args].concat());
        add(&["-b", "dirty", &dirty.to_string_lossy()])?;
        add(&["--detach", &detached.to_string_lossy()])?;
        add(&["--detach", &clean.to_string_lossy()])?;
        add(&["--detach", &pruned.to_string_lossy()])?;
        std::fs::remove_dir_all(&pruned)?;
        std::fs::write(dirty.join("README"), "edited")?;
        std::fs::write(dirty.join("new"), "untracked")?;
        setup.sandbox.commit(&detached, "a", "1")?;
        std::fs::write(setup.clone.join(".git").join("info").join("exclude"), ".claude/\n")?;
        let found = reasons(&setup)?;
        let problem = |path: &Path, problem| UnsafeReason::Worktree { path: path.to_path_buf(), problem };
        assert_eq!(found, vec![
            problem(&dirty.canonicalize()?, WorktreeProblem::UncommittedChanges),
            problem(&dirty.canonicalize()?, WorktreeProblem::UntrackedFiles),
            problem(&detached.canonicalize()?, WorktreeProblem::UnpushedCommits { count: 1 }),
        ]);
        Ok(())
    }

    #[test]
    fn every_reason_is_collected() -> anyhow::Result<()> {
        let setup = setup()?;
        setup.sandbox.commit(&setup.clone, "a", "1")?;
        std::fs::write(setup.clone.join("a"), "edited")?;
        std::fs::write(setup.clone.join("new"), "untracked")?;
        assert_eq!(reasons(&setup)?, vec![
            UnsafeReason::UncommittedChanges,
            UnsafeReason::UntrackedFiles,
            UnsafeReason::UnpushedCommits { count: 1 },
        ]);
        Ok(())
    }
}
