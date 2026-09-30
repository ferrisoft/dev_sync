//! Finds the repositories on disk (§8.1).

use std::ffi::OsStr;
use std::fs::DirEntry;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::domain;
use crate::git;
use crate::parallel;
use crate::workspace;


// ====================
// === ObservedRepo ===
// ====================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ObservedRepo {
    pub(crate) path: domain::RepoPath,
    pub(crate) id: domain::FileId,
    pub(crate) origin: git::Origin,
}


// ================
// === Leftover ===
// ================

pub(crate) const CLONING_PREFIX: &str = ".dev_sync-cloning-";
pub(crate) const MOVING_PREFIX: &str = ".dev_sync-moving-";

/// A temporary directory an interrupted run left behind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Leftover {
    /// A clone that never finished. Always ours, and safe to delete.
    Cloning(PathBuf),
    /// A repository parked half-way through a move. Holds real work; never deleted.
    Moving(PathBuf),
}

impl Leftover {
    pub(crate) fn path(&self) -> &Path {
        match self {
            Self::Cloning(path) | Self::Moving(path) => path,
        }
    }
}


// ===============
// === Scanned ===
// ===============

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Scanned {
    /// Sorted by path.
    pub(crate) repos: Vec<ObservedRepo>,
    /// Sorted by path.
    pub(crate) leftovers: Vec<Leftover>,
}


// ============
// === scan ===
// ============

/// Walks `root` without following symlinks and without entering repositories or hidden directories (a clone under a
/// hidden directory is how the user keeps it out of the layout). A directory whose `.git` is a file or a symlink — a
/// linked worktree or a submodule checkout — is skipped whole. Any directory that can't be read aborts the scan:
/// skipping it would make the repositories below it look removed, and the removal would spread to every machine. So
/// does another workspace below the root, whose clones would belong to both.
pub(crate) fn scan(git: &git::Git, root: &Path) -> anyhow::Result<Scanned> {
    let mut pending = vec![root.to_path_buf()];
    let mut repos = Vec::new();
    let mut leftovers = Vec::new();
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).with_context(|| format!("failed to read directory {}", dir.display()))?;
        let nested = dir != root && workspace::is_workspace(&dir);
        anyhow::ensure!(
            !nested,
            "{} holds another dev_sync workspace; workspaces can't be nested, so move it out of {}",
            dir.display(),
            root.display()
        );
        for entry in entries {
            match examine(entry, &dir)? {
                Found::Repo(repo) => repos.push(repo),
                Found::Directory(path) => pending.push(path),
                Found::Leftover(leftover) => leftovers.push(leftover),
                Found::Nothing => {}
            }
        }
    }
    let observed = parallel::map(&repos, git.policy().parallelism, |repo| observe(git, root, repo))?;
    let mut observed = observed.into_iter().collect::<anyhow::Result<Vec<_>>>()?;
    observed.sort_by(|left, right| left.path.cmp(&right.path));
    leftovers.sort_by(|left, right| left.path().cmp(right.path()));
    Ok(Scanned { repos: observed, leftovers })
}

struct FoundRepo {
    dir: PathBuf,
    id: domain::FileId,
}

enum Found {
    Repo(FoundRepo),
    Directory(PathBuf),
    Leftover(Leftover),
    Nothing,
}

fn examine(entry: std::io::Result<DirEntry>, dir: &Path) -> anyhow::Result<Found> {
    let entry = entry.with_context(|| format!("failed to read directory {}", dir.display()))?;
    let name = entry.file_name();
    let path = entry.path();
    let file_type = entry.file_type().with_context(|| format!("failed to read the type of {}", path.display()))?;
    let hidden = name.as_bytes().starts_with(b".");
    match (file_type.is_dir(), hidden) {
        (false, _) => Ok(Found::Nothing),
        (true, true) => Ok(leftover(&name, path).map_or(Found::Nothing, Found::Leftover)),
        (true, false) => {
            name.to_str().with_context(|| {
                format!("{} has a name that is not valid UTF-8; rename it so dev_sync can track it", path.display())
            })?;
            let dot_git = path.join(".git");
            match std::fs::symlink_metadata(&dot_git) {
                Ok(metadata) if metadata.is_dir() => {
                    Ok(Found::Repo(FoundRepo { id: domain::FileId::from_metadata(&metadata), dir: path }))
                }
                Ok(_) => Ok(Found::Nothing),
                Err(error) if error.kind() == ErrorKind::NotFound => Ok(Found::Directory(path)),
                Err(error) => Err(error).with_context(|| format!("failed to inspect {}", dot_git.display())),
            }
        }
    }
}

fn leftover(name: &OsStr, path: PathBuf) -> Option<Leftover> {
    let name = name.as_bytes();
    if name.starts_with(CLONING_PREFIX.as_bytes()) {
        Some(Leftover::Cloning(path))
    } else if name.starts_with(MOVING_PREFIX.as_bytes()) {
        Some(Leftover::Moving(path))
    } else {
        None
    }
}

fn observe(git: &git::Git, root: &Path, repo: &FoundRepo) -> anyhow::Result<ObservedRepo> {
    let path = domain::RepoPath::from_fs_path(root, &repo.dir)
        .with_context(|| format!("dev_sync can't track the repository at {}", repo.dir.display()))?;
    Ok(ObservedRepo { path, id: repo.id, origin: git::origin(git, &repo.dir)? })
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::PathBuf;

    use crate::domain;
    use crate::fixtures;
    use crate::git;
    use crate::workspace;
    use super::Leftover;
    use super::Scanned;
    use super::scan;

    struct Tree {
        sandbox: fixtures::Sandbox,
        root: PathBuf,
        remote: PathBuf,
    }

    impl Tree {
        fn new() -> anyhow::Result<Self> {
            let sandbox = fixtures::Sandbox::create()?;
            let remote = sandbox.remote("r")?;
            let root = sandbox.path().join("workspace");
            std::fs::create_dir(&root)?;
            Ok(Self { sandbox, root, remote })
        }

        fn clone_at(&self, relative: &str) -> anyhow::Result<PathBuf> {
            self.sandbox.clone(&self.remote, &self.root.join(relative))
        }

        fn scan(&self) -> anyhow::Result<Scanned> {
            scan(&fixtures::git(), &self.root)
        }

        fn paths(&self) -> anyhow::Result<Vec<String>> {
            Ok(self.scan()?.repos.into_iter().map(|repo| repo.path.to_string()).collect())
        }
    }

    #[test]
    fn finds_nested_repos_sorted_with_ids_and_origins() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        tree.clone_at("b/c")?;
        let a = tree.clone_at("a")?;
        let scanned = tree.scan()?;
        let paths = scanned.repos.iter().map(|repo| repo.path.to_string()).collect::<Vec<_>>();
        assert_eq!(paths, vec!["a", "b/c"]);
        let first = scanned.repos.first().ok_or_else(|| anyhow::anyhow!("no repos"))?;
        assert_eq!(first.id, domain::FileId::of(&a.join(".git"))?);
        assert_eq!(first.origin, git::Origin::Url(tree.remote.to_string_lossy().parse()?));
        assert_eq!(scanned.leftovers, vec![]);
        Ok(())
    }

    #[test]
    fn skips_hidden_folders_at_every_level() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        tree.clone_at(".hidden")?;
        tree.clone_at("x/.scratch/r")?;
        tree.clone_at("x/y")?;
        assert_eq!(tree.paths()?, vec!["x/y"]);
        Ok(())
    }

    #[test]
    fn a_workspace_below_the_root_is_an_error() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        fixtures::workspace(&tree.root)?;
        tree.clone_at("a")?;
        assert_eq!(tree.paths()?, vec!["a"]);
        let inner = tree.root.join("team");
        tree.sandbox.clone(&tree.remote, &inner.join(workspace::REPOSITORY_DIR))?;
        std::fs::write(inner.join(workspace::REPOSITORY_DIR).join(workspace::LAYOUT_FILE), "format = 1\n")?;
        tree.clone_at("team/lib")?;
        let message = tree.scan().err().map(|error| format!("{error:#}")).unwrap_or_default();
        assert!(message.contains("team holds another dev_sync workspace"), "{message}");
        Ok(())
    }

    #[test]
    fn skips_directories_whose_git_is_a_file() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        let main = tree.clone_at("main")?;
        let linked = tree.root.join("linked");
        tree.sandbox.git(&main, &["worktree", "add", "--quiet", "-b", "x", &linked.to_string_lossy()])?;
        std::fs::create_dir_all(linked.join("inner"))?;
        tree.clone_at("linked/inner/r")?;
        assert_eq!(tree.paths()?, vec!["main"]);
        Ok(())
    }

    #[test]
    fn does_not_descend_into_repositories() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        let outer = tree.clone_at("outer")?;
        tree.sandbox.clone(&tree.remote, &outer.join("inner"))?;
        assert_eq!(tree.paths()?, vec!["outer"]);
        Ok(())
    }

    #[test]
    fn does_not_follow_symlinks() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        let elsewhere = tree.sandbox.path().join("elsewhere");
        tree.sandbox.clone(&tree.remote, &elsewhere.join("r"))?;
        std::os::unix::fs::symlink(&elsewhere, tree.root.join("link"))?;
        std::os::unix::fs::symlink(elsewhere.join("r"), tree.root.join("repo-link"))?;
        assert_eq!(tree.paths()?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn reports_a_repository_without_origin() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        let local = tree.root.join("local");
        std::fs::create_dir(&local)?;
        tree.sandbox.git(&local, &["init", "--quiet"])?;
        let scanned = tree.scan()?;
        let origins = scanned.repos.iter().map(|repo| repo.origin.clone()).collect::<Vec<_>>();
        assert_eq!(origins, vec![git::Origin::Missing]);
        Ok(())
    }

    #[test]
    fn an_unreadable_directory_is_an_error() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        let locked = tree.root.join("locked");
        std::fs::create_dir(&locked)?;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))?;
        let privileged = std::fs::read_dir(&locked).is_ok();
        let result = tree.scan();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
        if !privileged {
            let message = result.err().map(|error| format!("{error:#}")).unwrap_or_default();
            assert!(message.contains("locked"), "{message}");
        }
        Ok(())
    }

    #[test]
    fn a_non_utf8_directory_name_is_an_error() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        std::fs::create_dir(tree.root.join(std::ffi::OsStr::from_bytes(b"bad\xffname")))?;
        std::fs::write(tree.root.join(std::ffi::OsStr::from_bytes(b"file\xff")), "a file is fine")?;
        assert!(tree.scan().is_err());
        Ok(())
    }

    #[test]
    fn collects_leftover_temp_directories() -> anyhow::Result<()> {
        let tree = Tree::new()?;
        let cloning = tree.root.join("a").join(".dev_sync-cloning-r-123");
        let moving = tree.root.join(".dev_sync-moving-9-0");
        std::fs::create_dir_all(&cloning)?;
        std::fs::create_dir_all(&moving)?;
        std::fs::create_dir_all(tree.root.join(".dev_sync-other"))?;
        let scanned = tree.scan()?;
        assert_eq!(scanned.leftovers, vec![Leftover::Moving(moving), Leftover::Cloning(cloning)]);
        Ok(())
    }
}
