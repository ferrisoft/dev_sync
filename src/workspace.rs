//! The workspace: a folder of clones whose layout lives in a git repository hidden in its `.dev_sync` folder. Finding
//! and creating it, and the git operations on that repository.

use std::ffi::OsStr;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::domain;
use crate::git;
use crate::layout;
use crate::record;
use crate::shell;


// =================
// === Workspace ===
// =================

pub(crate) const LAYOUT_FILE: &str = "repos.toml";
/// The hidden folder at the workspace root that holds the workspace repository.
pub(crate) const REPOSITORY_DIR: &str = ".dev_sync";

/// A dev_sync workspace: a folder of clones (the root) whose layout lives in the workspace repository, hidden in the
/// root's `.dev_sync` folder. The root itself is not a git repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Workspace {
    /// Canonical, so every path below it is too.
    root: PathBuf,
    repository: Repository,
    git_dir: PathBuf,
}

impl Workspace {
    /// Uses `explicit` when given; otherwise the nearest directory at or above the current one that holds a workspace
    /// repository.
    pub(crate) fn discover(git: &git::Git, explicit: Option<&Path>) -> anyhow::Result<Self> {
        let root = match explicit {
            Some(dir) => {
                let dir = std::path::absolute(dir).with_context(|| format!("failed to resolve {}", dir.display()))?;
                anyhow::ensure!(
                    is_workspace(&dir),
                    "{} is not a dev_sync workspace (it needs {REPOSITORY_DIR}/{LAYOUT_FILE})",
                    dir.display()
                );
                dir
            }
            None => {
                let cwd = std::env::current_dir().context("failed to read the current directory")?;
                cwd.ancestors().find(|dir| is_workspace(dir)).map(Path::to_path_buf).with_context(|| {
                    format!(
                        "not inside a dev_sync workspace (no {REPOSITORY_DIR} folder found above {}); pass `--root`, \
                         start one with `dev_sync init <dir>`, or clone a workspace repository into \
                         <dir>/{REPOSITORY_DIR}",
                        cwd.display()
                    )
                })?
            }
        };
        let root = root.canonicalize().with_context(|| format!("failed to resolve {}", root.display()))?;
        let repository = Repository::of(&root);
        let output = git.at(repository.dir()).args(["rev-parse", "--absolute-git-dir"]).run_ok(git::Access::Read)?;
        let git_dir = PathBuf::from(OsStr::from_bytes(output.strip_suffix(b"\n").unwrap_or(&output)));
        Ok(Self { root, repository, git_dir })
    }

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn repository(&self) -> &Repository {
        &self.repository
    }

    pub(crate) fn state_file(&self) -> PathBuf {
        self.git_dir.join("dev_sync").join("state.toml")
    }

    pub(crate) fn lock_file(&self) -> PathBuf {
        self.git_dir.join("dev_sync").join("lock")
    }

    /// The layout text being committed, kept until the commit is done: a run killed in between leaves it for the next
    /// one to find.
    fn pending_commit_file(&self) -> PathBuf {
        self.git_dir.join("dev_sync").join("committing")
    }
}

/// Whether `dir` is the root of a workspace: its `.dev_sync` folder holds `repos.toml` and a git repository.
pub(crate) fn is_workspace(dir: &Path) -> bool {
    let repository = Repository::of(dir);
    repository.layout_file().is_file() && std::fs::symlink_metadata(repository.dir().join(".git")).is_ok()
}


// ==================
// === Repository ===
// ==================

/// The workspace repository: `<root>/.dev_sync`, the git repository whose work tree holds `repos.toml`. A type of its
/// own, so a git command meant for it can't be run on the root or on a clone by mistake.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Repository {
    dir: PathBuf,
}

impl Repository {
    /// The workspace repository of the workspace rooted at `root`.
    pub(crate) fn of(root: &Path) -> Self {
        Self { dir: root.join(REPOSITORY_DIR) }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn layout_file(&self) -> PathBuf {
        self.dir.join(LAYOUT_FILE)
    }

    /// The directory quoted for sh, for `git -C` in hints.
    pub(crate) fn shell_word(&self) -> String {
        shell::word(&self.dir.to_string_lossy())
    }
}


// ================
// === Creation ===
// ================

pub(crate) const README_FILE: &str = "README.md";
const GITATTRIBUTES: &str = include_str!("../templates/gitattributes");
const README_TEMPLATE: &str = include_str!("../templates/workspace-readme.md");

/// The workspace `dir` lies inside, `dir` itself not counted.
pub(crate) fn outer_workspace(dir: &Path) -> Option<&Path> {
    dir.ancestors().skip(1).find(|ancestor| is_workspace(ancestor))
}

/// The README of a workspace repository published at `remote`: what the repository is, what its files hold, and how to
/// set up another machine from it.
pub(crate) fn readme(remote: &domain::RemoteUrl) -> String {
    README_TEMPLATE.replace("__REMOTE__", &shell::word(remote.as_str()))
}

/// Fills a workspace repository that has no commit yet, as right after cloning an empty one: an empty `repos.toml`,
/// `.gitattributes` and the README, committed, with the merge driver registered.
pub(crate) fn populate(git: &git::Git, repository: &Repository, remote: &domain::RemoteUrl) -> anyhow::Result<()> {
    let files = [
        (".gitattributes", GITATTRIBUTES.to_owned()),
        (LAYOUT_FILE, layout::render(&layout::Layout::default())),
        (README_FILE, readme(remote)),
    ];
    for (name, content) in &files {
        std::fs::write(repository.dir().join(name), content).with_context(|| format!("failed to write {name}"))?;
    }
    register_merge_driver(git, repository)?;
    let added = git.at(repository.dir()).args(["add", "--"]).args(files.iter().map(|(name, _)| name));
    added.run_ok(git::Access::Write)?;
    let committed = git.at(repository.dir()).args(["commit", "--quiet", "-m", "init dev_sync workspace"]);
    committed.run_ok(git::Access::Lengthy).map(|_| ())
}

/// Adds the README to a workspace repository made before there was one. True when it did.
pub(crate) fn add_missing_readme(
    git: &git::Git,
    repository: &Repository,
    remote: &domain::RemoteUrl,
) -> anyhow::Result<bool> {
    let path = repository.dir().join(README_FILE);
    match std::fs::symlink_metadata(&path).is_ok() {
        true => Ok(false),
        false => {
            std::fs::write(&path, readme(remote)).with_context(|| format!("failed to write {}", path.display()))?;
            let dir = repository.dir();
            git.at(dir).args(["add", "--", README_FILE]).run_ok(git::Access::Write)?;
            let committed = git.at(dir).args(["commit", "--quiet", "-m", "add README.md", "--", README_FILE]);
            committed.run_ok(git::Access::Lengthy).map(|_| true)
        }
    }
}

/// Whether the repository has no commit yet.
pub(crate) fn is_unborn(git: &git::Git, repository: &Repository) -> anyhow::Result<bool> {
    let finished = git.at(repository.dir()).args(["rev-parse", "--verify", "--quiet", "HEAD"]).run(git::Access::Read)?;
    match finished.code {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(anyhow::anyhow!("failed to read HEAD of {} ({})", repository.dir().display(), finished.exit())),
    }
}

pub(crate) fn add_origin(git: &git::Git, repository: &Repository, url: &domain::RemoteUrl) -> anyhow::Result<()> {
    let added = git.at(repository.dir()).args(["remote", "add", "--", "origin", url.as_str()]);
    added.run_ok(git::Access::Write).map(|_| ())
}


// ===================
// === MergeDriver ===
// ===================

const DRIVER_NAME: &str = "dev_sync layout merge";

/// The command git runs to merge `repos.toml`: `executable`'s `merge-driver`, quoted for sh.
pub(crate) fn driver_command(executable: &Path) -> anyhow::Result<String> {
    Ok(format!("{} merge-driver %O %A %B %P", driver_word(executable)?))
}

/// `path` quoted for sh, with `%` doubled: git expands `%O`, `%A`, `%B`, `%L` and `%P` anywhere in the command.
fn driver_word(path: &Path) -> anyhow::Result<String> {
    path.to_str().map(|text| shell::quote(text).replace('%', "%%")).with_context(|| {
        format!("{} is not valid UTF-8, so git can't run the merge driver from it", path.display())
    })
}

/// Writes the driver, naming the running executable, into the workspace repository's own config, only when it
/// differs.
pub(crate) fn register_merge_driver(git: &git::Git, repository: &Repository) -> anyhow::Result<()> {
    let executable = std::env::current_exe().context("failed to find the dev_sync executable")?;
    set_config(git, repository, "merge.dev-sync.name", DRIVER_NAME)?;
    set_config(git, repository, "merge.dev-sync.driver", &driver_command(&executable)?)
}

fn set_config(git: &git::Git, repository: &Repository, key: &str, value: &str) -> anyhow::Result<()> {
    let current = git.at(repository.dir()).args(["config", "--local", "--get", key]).run(git::Access::Read)?;
    let unchanged = current.code == Some(0) && current.stdout.strip_suffix(b"\n") == Some(value.as_bytes());
    if !unchanged {
        git.at(repository.dir()).args(["config", "--local", key, value]).run_ok(git::Access::Write)?;
    }
    Ok(())
}


// ===============
// === Queries ===
// ===============

/// `None` when HEAD is detached. Reads the full ref, because `--short` turns `main` into `heads/main` when a tag of
/// the same name exists.
pub(crate) fn current_branch(git: &git::Git, repository: &Repository) -> anyhow::Result<Option<domain::BranchName>> {
    let finished = git.at(repository.dir()).args(["symbolic-ref", "--quiet", "HEAD"]).run(git::Access::Read)?;
    match finished.code {
        Some(0) => {
            let full = String::from_utf8_lossy(&finished.stdout).trim().to_owned();
            full.strip_prefix("refs/heads/").map(str::parse).transpose()
        }
        Some(1) => Ok(None),
        _ => Err(anyhow::anyhow!(
            "failed to read the workspace branch ({}): {}",
            finished.exit(),
            finished.error_text()
        )),
    }
}

pub(crate) fn upstream(
    git: &git::Git,
    repository: &Repository,
    branch: &domain::BranchName,
) -> anyhow::Result<Option<git::Upstream>> {
    let branches = git::branches(git, repository.dir())?;
    Ok(branches.into_iter().find(|info| info.name == *branch).and_then(|info| info.upstream))
}

/// Refuses a workspace branch whose upstream is on a remote named like an option.
pub(crate) fn unusable_remote(name: &str, branch: &domain::BranchName) -> anyhow::Error {
    anyhow::anyhow!(
        "the workspace branch {branch} tracks a remote named {name:?}, which git would take for an option; set its \
         upstream to a proper remote (`git branch --set-upstream-to=origin/{branch}`)"
    )
}

pub(crate) fn has_remote(git: &git::Git, repository: &Repository, name: &str) -> anyhow::Result<bool> {
    let key = format!("remote.{name}.url");
    Ok(git.at(repository.dir()).args(["config", "--get", &key]).run(git::Access::Read)?.code == Some(0))
}

pub(crate) fn ref_exists(git: &git::Git, repository: &Repository, full_ref: &str) -> anyhow::Result<bool> {
    let verified = git.at(repository.dir()).args(["rev-parse", "--verify", "--quiet", full_ref]);
    Ok(verified.run(git::Access::Read)?.code == Some(0))
}

pub(crate) fn merge_in_progress(git: &git::Git, repository: &Repository) -> anyhow::Result<bool> {
    let dir = repository.dir();
    let output = git.at(dir).args(["rev-parse", "--git-path", "MERGE_HEAD"]).run_ok(git::Access::Read)?;
    let path = dir.join(OsStr::from_bytes(output.strip_suffix(b"\n").unwrap_or(&output)));
    Ok(std::fs::symlink_metadata(path).is_ok())
}

/// Whether the working copy or the index of `repos.toml` differs from HEAD. Header entries, like the `# stash` one
/// that `status.showStash` adds, say nothing about the file.
pub(crate) fn layout_modified(git: &git::Git, repository: &Repository) -> anyhow::Result<bool> {
    let output = git
        .at(repository.dir())
        .args(["status", "--porcelain=v2", "-z", "--untracked-files=no", "--", LAYOUT_FILE])
        .run_ok(git::Access::Read)?;
    Ok(output.split(|byte| *byte == 0).any(|entry| !entry.is_empty() && !entry.starts_with(b"# ")))
}

/// The layout as committed at `rev`.
pub(crate) fn snapshot(git: &git::Git, repository: &Repository, rev: &str) -> anyhow::Result<layout::Layout> {
    let object = format!("{rev}:{LAYOUT_FILE}");
    let blob = git.at(repository.dir()).args(["cat-file", "blob", &object]).run_ok(git::Access::Read)?;
    let text = String::from_utf8(blob).with_context(|| format!("{LAYOUT_FILE} at {rev} is not valid UTF-8"))?;
    layout::parse(&text).with_context(|| format!("{LAYOUT_FILE} at {rev} is invalid"))
}

/// The commit `HEAD` and `other` share, if any.
pub(crate) fn merge_base(
    git: &git::Git,
    repository: &Repository,
    other: &str,
) -> anyhow::Result<Option<domain::CommitId>> {
    let finished = git.at(repository.dir()).args(["merge-base", "HEAD", other]).run(git::Access::Read)?;
    match finished.code {
        Some(0) => String::from_utf8_lossy(&finished.stdout).trim().parse().map(Some),
        Some(1) => Ok(None),
        _ => Err(anyhow::anyhow!(
            "failed to find the merge base with {other} ({}): {}",
            finished.exit(),
            finished.error_text()
        )),
    }
}

/// Whether `repos.toml` ever listed `repo` in HEAD's history: a commit added or removed its line.
pub(crate) fn listed_before(
    git: &git::Git,
    repository: &Repository,
    repo: &layout::LayoutRepo,
) -> anyhow::Result<bool> {
    let entry = layout::render_entry(repo);
    let commits = git
        .at(repository.dir())
        .args(["log", "--format=%H", "-S"])
        .arg(&entry)
        .args(["HEAD", "--", LAYOUT_FILE])
        .run_ok(git::Access::Read)?;
    Ok(!commits.is_empty())
}

pub(crate) fn unmerged_paths(git: &git::Git, repository: &Repository) -> anyhow::Result<Vec<String>> {
    let listed = git.at(repository.dir()).args(["diff", "--name-only", "--diff-filter=U", "-z"]);
    let output = listed.run_ok(git::Access::Read)?;
    let names = output.split(|byte| *byte == 0).filter(|name| !name.is_empty());
    Ok(names.map(|name| String::from_utf8_lossy(name).into_owned()).collect())
}


// ===============
// === Changes ===
// ===============

/// Writes `repos.toml` in canonical form, atomically.
pub(crate) fn write_layout(repository: &Repository, layout: &layout::Layout) -> anyhow::Result<()> {
    write_layout_text(repository, &layout::render(layout))
}

/// Replaces `repos.toml` with `text`, atomically.
pub(crate) fn write_layout_text(repository: &Repository, text: &str) -> anyhow::Result<()> {
    let staged = repository.dir().join(format!(".{LAYOUT_FILE}.tmp"));
    let write = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&staged)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&staged, repository.layout_file())
    };
    write().with_context(|| format!("failed to write {}", repository.layout_file().display()))
}

/// Writes and commits `repos.toml` alone. If the commit fails, the file goes back to HEAD's version, so a failed run
/// never leaves uncommitted edits behind; if the run is killed instead, `recover_interrupted_commit` does that next
/// time.
pub(crate) fn commit_layout(
    git: &git::Git,
    workspace: &Workspace,
    layout: &layout::Layout,
    message: &record::CommitMessage,
) -> anyhow::Result<()> {
    let repository = &workspace.repository;
    let text = layout::render(layout);
    let pending = workspace.pending_commit_file();
    let noted = pending.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&pending, &text));
    noted.with_context(|| format!("failed to write {}", pending.display()))?;
    write_layout_text(repository, &text)?;
    let mut paragraphs = vec!["-m", message.subject.as_str()];
    if !message.body.is_empty() {
        paragraphs.extend(["-m", message.body.as_str()]);
    }
    let committed = git
        .at(repository.dir())
        .arg("commit")
        .arg("--quiet")
        .args(paragraphs)
        .args(["--", LAYOUT_FILE])
        .run_ok(git::Access::Lengthy);
    let settled = committed.map(|_| ()).map_err(|error| match restore_layout(git, repository) {
        Ok(()) => error.context("failed to commit the layout"),
        Err(restore_error) => error.context(format!(
            "failed to commit the layout, and to restore {LAYOUT_FILE} ({restore_error:#}); discard it with \
             `git -C {} checkout -- {LAYOUT_FILE}`",
            repository.shell_word()
        )),
    });
    settled.and_then(|()| remove_pending(&pending))
}

/// Undoes what a run killed while committing `repos.toml` left behind: the file still holds exactly the text that run
/// wrote, uncommitted. Its base wasn't saved, so the next record finds the same changes again. A `repos.toml` edited
/// since is left alone. True when it restored the file.
pub(crate) fn recover_interrupted_commit(git: &git::Git, workspace: &Workspace) -> anyhow::Result<bool> {
    let pending = workspace.pending_commit_file();
    let repository = &workspace.repository;
    match std::fs::read_to_string(&pending) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to read {}", pending.display())),
        Ok(text) => {
            let current = std::fs::read_to_string(repository.layout_file()).ok();
            let ours = current.as_deref() == Some(text.as_str()) && layout_modified(git, repository)?;
            if ours {
                restore_layout(git, repository)?;
            }
            remove_pending(&pending)?;
            Ok(ours)
        }
    }
}

fn restore_layout(git: &git::Git, repository: &Repository) -> anyhow::Result<()> {
    let restored = git.at(repository.dir()).args(["checkout", "HEAD", "--", LAYOUT_FILE]);
    restored.run_ok(git::Access::Write).map(|_| ())
}

fn remove_pending(pending: &Path) -> anyhow::Result<()> {
    match std::fs::remove_file(pending) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            Err(error).with_context(|| format!("failed to remove {}", pending.display()))
        }
        Ok(()) | Err(_) => Ok(()),
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::fixtures;
    use super::LAYOUT_FILE;
    use super::README_FILE;
    use super::REPOSITORY_DIR;
    use super::Repository;
    use super::Workspace;
    use super::add_missing_readme;
    use super::current_branch;
    use super::driver_command;
    use super::layout_modified;
    use super::outer_workspace;

    fn failure<T>(result: anyhow::Result<T>) -> String {
        result.err().map(|error| format!("{error:#}")).unwrap_or_default()
    }

    #[test]
    fn the_driver_names_the_executable_and_escapes_percent_signs() -> anyhow::Result<()> {
        let plain = driver_command(Path::new("/nix/store/x/bin/dev_sync"))?;
        assert_eq!(plain, "'/nix/store/x/bin/dev_sync' merge-driver %O %A %B %P");
        let odd = driver_command(Path::new("/bin/100%P/it's"))?;
        assert_eq!(odd, "'/bin/100%%P/it'\\''s' merge-driver %O %A %B %P");
        Ok(())
    }

    #[test]
    fn discovery_finds_the_repository_hidden_in_the_root() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let git = fixtures::git();
        let dir = sandbox.path().join("dev");
        std::fs::create_dir_all(dir.join("a").join("b"))?;
        let root = fixtures::workspace(&dir)?;
        assert!(!root.join(".git").exists() && !root.join(LAYOUT_FILE).exists());
        let workspace = Workspace::discover(&git, Some(&dir.join("a").join("..")))?;
        assert_eq!(workspace.root, root);
        assert_eq!(workspace.repository.dir(), root.join(REPOSITORY_DIR));
        assert_eq!(workspace.git_dir, root.join(REPOSITORY_DIR).join(".git"));
        assert!(failure(Workspace::discover(&git, Some(&dir.join("a")))).contains("is not a dev_sync workspace"));
        assert!(Workspace::discover(&git, Some(&root.join(REPOSITORY_DIR))).is_err());
        Ok(())
    }

    #[test]
    fn the_outer_workspace_is_found_above_but_never_at_the_folder_itself() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let root = fixtures::workspace(&sandbox.path().join("dev"))?;
        assert_eq!(outer_workspace(&root.join("team").join("x")), Some(root.as_path()));
        assert_eq!(outer_workspace(&root), None);
        assert_eq!(outer_workspace(sandbox.path()), None);
        Ok(())
    }

    #[test]
    fn the_readme_says_how_to_join_and_is_added_only_where_missing() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let repository = Repository::of(&fixtures::workspace(&sandbox.path().join("dev"))?);
        let readme = std::fs::read_to_string(repository.dir().join(README_FILE))?;
        assert!(readme.contains("dev_sync init ~/dev --remote https://example.invalid/dev.git"), "{readme}");
        let remote = "git@github.com:you/it's.git".parse()?;
        assert!(!add_missing_readme(&fixtures::git(), &repository, &remote)?);
        sandbox.git(repository.dir(), &["rm", "--quiet", README_FILE])?;
        sandbox.git(repository.dir(), &["commit", "--quiet", "-m", "older"])?;
        assert!(add_missing_readme(&fixtures::git(), &repository, &remote)?);
        let committed = sandbox.git(repository.dir(), &["show", &format!("HEAD:{README_FILE}")])?;
        assert!(committed.contains("--remote 'git@github.com:you/it'\\''s.git'"), "{committed}");
        Ok(())
    }

    #[test]
    fn reads_the_branch_even_when_a_tag_has_its_name() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let repository = Repository::of(&fixtures::workspace(&sandbox.path().join("dev"))?);
        sandbox.git(repository.dir(), &["tag", "main"])?;
        assert_eq!(current_branch(&fixtures::git(), &repository)?, Some("main".parse()?));
        sandbox.git(repository.dir(), &["checkout", "--quiet", "--detach"])?;
        assert_eq!(current_branch(&fixtures::git(), &repository)?, None);
        Ok(())
    }

    #[test]
    fn a_stash_shown_by_status_is_no_layout_edit() -> anyhow::Result<()> {
        let sandbox = fixtures::Sandbox::create()?;
        let repository = Repository::of(&fixtures::workspace(&sandbox.path().join("dev"))?);
        assert!(!layout_modified(&fixtures::git(), &repository)?);
        std::fs::write(repository.dir().join(".gitattributes"), "stashed\n")?;
        sandbox.git(repository.dir(), &["stash", "--quiet"])?;
        sandbox.git(repository.dir(), &["config", "status.showStash", "true"])?;
        assert!(!layout_modified(&fixtures::git(), &repository)?);
        std::fs::write(repository.layout_file(), "# edited\n")?;
        assert!(layout_modified(&fixtures::git(), &repository)?);
        Ok(())
    }
}
