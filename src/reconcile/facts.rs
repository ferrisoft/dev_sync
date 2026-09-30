//! What is on disk at each place the layout wants a repository, gathered before planning so the planner stays pure.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::fs::Metadata;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;

use anyhow::Context as _;

use crate::domain;


// ================
// === Obstacle ===
// ================

/// Something in the way of a clone or a move.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Obstacle {
    File,
    Symlink,
    /// A directory holding something besides directories and repositories: a file, a symlink, a hidden entry, a
    /// linked worktree or a submodule checkout.
    NonEmptyDirectory,
    Repository,
    /// A directory whose `.git` is a file or symlink: a linked worktree or a submodule checkout.
    GitLink,
    /// A file or symlink where a parent directory should be.
    AncestorNotDirectory { ancestor: domain::RepoPath },
}

impl fmt::Display for Obstacle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File => f.write_str("a file is in the way"),
            Self::Symlink => f.write_str("a symlink is in the way"),
            Self::NonEmptyDirectory => f.write_str("a non-empty directory is in the way"),
            Self::Repository => f.write_str("another repository is in the way"),
            Self::GitLink => f.write_str("a linked worktree or submodule checkout is in the way"),
            Self::AncestorNotDirectory { ancestor } => write!(f, "`{ancestor}` is a file or symlink, not a directory"),
        }
    }
}


// =======================
// === DestinationFact ===
// =======================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DestinationFact {
    Free,
    /// A directory holding nothing but directories and these repositories (none when it is empty). It can take a
    /// repository once they are gone; the empty directories left are removed.
    Directory { repos: BTreeSet<domain::RepoPath> },
    Occupied { obstacle: Obstacle },
    /// A proper ancestor is a repository (or a worktree) on disk.
    InsideRepo { repo: domain::RepoPath },
}

enum Ancestor {
    Directory,
    Missing,
    Blocks(DestinationFact),
}

pub(crate) fn fact(root: &Path, path: &domain::RepoPath) -> anyhow::Result<DestinationFact> {
    let verdict = path
        .proper_ancestors()
        .into_iter()
        .find_map(|ancestor| match inspect_ancestor(root, ancestor) {
            Ok(Ancestor::Directory) => None,
            Ok(Ancestor::Missing) => Some(Ok(DestinationFact::Free)),
            Ok(Ancestor::Blocks(fact)) => Some(Ok(fact)),
            Err(error) => Some(Err(error)),
        })
        .transpose()?;
    verdict.map_or_else(|| leaf_fact(root, path), Ok)
}

fn inspect_ancestor(root: &Path, ancestor: domain::RepoPath) -> anyhow::Result<Ancestor> {
    let dir = ancestor.to_fs_path(root);
    Ok(match metadata(&dir)? {
        None => Ancestor::Missing,
        Some(found) if !found.is_dir() => {
            Ancestor::Blocks(DestinationFact::Occupied { obstacle: Obstacle::AncestorNotDirectory { ancestor } })
        }
        Some(_) => match metadata(&dir.join(".git"))? {
            Some(_) => Ancestor::Blocks(DestinationFact::InsideRepo { repo: ancestor }),
            None => Ancestor::Directory,
        },
    })
}

fn leaf_fact(root: &Path, path: &domain::RepoPath) -> anyhow::Result<DestinationFact> {
    let dir = path.to_fs_path(root);
    let occupied = |obstacle| DestinationFact::Occupied { obstacle };
    Ok(match metadata(&dir)? {
        None => DestinationFact::Free,
        Some(found) if found.file_type().is_symlink() => occupied(Obstacle::Symlink),
        Some(found) if !found.is_dir() => occupied(Obstacle::File),
        Some(_) => match metadata(&dir.join(".git"))? {
            Some(git) if git.is_dir() => occupied(Obstacle::Repository),
            Some(_) => occupied(Obstacle::GitLink),
            None => match repos_within(root, &dir)? {
                Some(repos) => DestinationFact::Directory { repos },
                None => occupied(Obstacle::NonEmptyDirectory),
            },
        },
    })
}

/// The repositories in `dir`, found through plain directories without entering the repositories; `None` as soon as
/// anything else turns up.
fn repos_within(root: &Path, dir: &Path) -> anyhow::Result<Option<BTreeSet<domain::RepoPath>>> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("failed to read directory {}", dir.display()))?;
    let found = entries
        .map(|entry| {
            let entry = entry.with_context(|| format!("failed to read directory {}", dir.display()))?;
            repos_at(root, &entry.path())
        })
        .collect::<anyhow::Result<Option<Vec<_>>>>()?;
    Ok(found.map(|sets| sets.into_iter().flatten().collect()))
}

fn repos_at(root: &Path, path: &Path) -> anyhow::Result<Option<BTreeSet<domain::RepoPath>>> {
    let hidden = path.file_name().is_some_and(|name| name.as_bytes().starts_with(b"."));
    let repo = || domain::RepoPath::from_fs_path(root, path).ok().map(|repo| BTreeSet::from([repo]));
    match metadata(path)? {
        Some(found) if found.is_dir() && !hidden => match metadata(&path.join(".git"))? {
            Some(git) if git.is_dir() => Ok(repo()),
            Some(_) => Ok(None),
            None => repos_within(root, path),
        },
        Some(_) | None => Ok(None),
    }
}


// =================
// === DiskFacts ===
// =================

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct DiskFacts {
    pub(crate) destinations: BTreeMap<domain::RepoPath, DestinationFact>,
}

/// Looks at every given path, and at its ancestors from the root down.
pub(crate) fn collect_facts<'a>(
    root: &Path,
    paths: impl Iterator<Item = &'a domain::RepoPath>,
) -> anyhow::Result<DiskFacts> {
    let destinations = paths.map(|path| Ok((path.clone(), fact(root, path)?))).collect::<anyhow::Result<_>>()?;
    Ok(DiskFacts { destinations })
}


// ================
// === metadata ===
// ================

/// `None` when nothing is at `path`. Symlinks aren't followed.
pub(crate) fn metadata(path: &Path) -> anyhow::Result<Option<Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(found) => Ok(Some(found)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to inspect {}", path.display())),
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use crate::fixtures;
    use super::DestinationFact;
    use super::Obstacle;
    use super::collect_facts;

    #[test]
    fn describes_every_kind_of_destination() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        let make = |relative: &str| std::fs::create_dir_all(root.join(relative));
        make("empty")?;
        make("hollow/a/b")?;
        make("full/stuff")?;
        std::fs::write(root.join("full").join("stuff").join("notes"), "x")?;
        make("holder/r/.git")?;
        make("holder/s/t/.git")?;
        make("holder/s/u")?;
        make("mixed/r/.git")?;
        std::fs::write(root.join("mixed").join("notes"), "x")?;
        make("hidden/.cache")?;
        make("linked/w")?;
        std::fs::write(root.join("linked").join("w").join(".git"), "gitdir: /elsewhere")?;
        make("repo/.git")?;
        make("repo/sub")?;
        make("worktree")?;
        std::fs::write(root.join("worktree").join(".git"), "gitdir: /elsewhere")?;
        std::fs::write(root.join("file"), "x")?;
        std::os::unix::fs::symlink(root.join("full"), root.join("link"))?;
        let paths = [
            "new", "new/deeper", "empty", "hollow", "full", "holder", "mixed", "hidden", "linked", "repo", "repo/sub",
            "repo/sub/x", "worktree", "file", "file/x", "link", "link/x",
        ]
        .into_iter()
        .map(fixtures::path)
        .collect::<anyhow::Result<Vec<_>>>()?;
        let facts = collect_facts(root, paths.iter())?;
        let fact = |path: &str| -> anyhow::Result<DestinationFact> {
            facts.destinations.get(&fixtures::path(path)?).cloned().ok_or_else(|| anyhow::anyhow!("no fact for {path}"))
        };
        let occupied = |obstacle| DestinationFact::Occupied { obstacle };
        let directory = |repos: &[&str]| -> anyhow::Result<DestinationFact> {
            let repos = repos.iter().copied().map(fixtures::path).collect::<anyhow::Result<_>>()?;
            Ok(DestinationFact::Directory { repos })
        };
        assert_eq!(fact("new")?, DestinationFact::Free);
        assert_eq!(fact("new/deeper")?, DestinationFact::Free);
        assert_eq!(fact("empty")?, directory(&[])?);
        assert_eq!(fact("hollow")?, directory(&[])?);
        assert_eq!(fact("full")?, occupied(Obstacle::NonEmptyDirectory));
        assert_eq!(fact("holder")?, directory(&["holder/r", "holder/s/t"])?);
        assert_eq!(fact("mixed")?, occupied(Obstacle::NonEmptyDirectory));
        assert_eq!(fact("hidden")?, occupied(Obstacle::NonEmptyDirectory));
        assert_eq!(fact("linked")?, occupied(Obstacle::NonEmptyDirectory));
        assert_eq!(fact("repo")?, occupied(Obstacle::Repository));
        assert_eq!(fact("repo/sub")?, DestinationFact::InsideRepo { repo: fixtures::path("repo")? });
        assert_eq!(fact("repo/sub/x")?, DestinationFact::InsideRepo { repo: fixtures::path("repo")? });
        assert_eq!(fact("worktree")?, occupied(Obstacle::GitLink));
        assert_eq!(fact("file")?, occupied(Obstacle::File));
        assert_eq!(fact("file/x")?, occupied(Obstacle::AncestorNotDirectory { ancestor: fixtures::path("file")? }));
        assert_eq!(fact("link")?, occupied(Obstacle::Symlink));
        assert_eq!(fact("link/x")?, occupied(Obstacle::AncestorNotDirectory { ancestor: fixtures::path("link")? }));
        Ok(())
    }
}
