//! The tree `list` prints: a dev folder's folders down to its repositories, with notes on each repository.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;
use std::path::PathBuf;

use crate::git;


// =============
// === Entry ===
// =============

/// A folder in the tree, named as it appears under its parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) path: PathBuf,
    pub(crate) kind: Kind,
}

impl Entry {
    /// The repositories in the tree, in the order they are drawn.
    pub(crate) fn repositories(&self) -> Vec<PathBuf> {
        match &self.kind {
            Kind::Repository(_) => vec![self.path.clone()],
            Kind::Folder(children) => children.iter().flat_map(Self::repositories).collect(),
            Kind::LinkedCheckout | Kind::NoRepositories | Kind::Unreadable(_) => Vec::new(),
        }
    }

    /// The tree with each repository's notes taken out of `notes`, by path.
    pub(crate) fn with_notes(self, notes: &mut BTreeMap<PathBuf, Vec<Note>>) -> Self {
        let kind = match self.kind {
            Kind::Repository(own) => Kind::Repository(notes.remove(&self.path).unwrap_or(own)),
            Kind::Folder(children) => Kind::Folder(children.into_iter().map(|child| child.with_notes(notes)).collect()),
            kind @ (Kind::LinkedCheckout | Kind::NoRepositories | Kind::Unreadable(_)) => kind,
        };
        Self { kind, ..self }
    }
}

/// What a folder turned out to be.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    /// A repository, with what `list` says about it.
    Repository(Vec<Note>),
    /// A folder whose `.git` is a file or a symlink: a linked worktree or a submodule checkout. Never synced.
    LinkedCheckout,
    /// A folder with something to show inside; its folders, sorted by name.
    Folder(Vec<Entry>),
    /// A folder with no repository anywhere inside, so nothing in it is synced.
    NoRepositories,
    /// A folder that couldn't be read, with the reason.
    Unreadable(String),
}

/// The entry for the folder at `path`. The walk goes no deeper than a repository, and leaves out hidden entries (as the
/// scan does), files and symlinks. A folder whose folders all lack repositories is one entry: nothing inside it is
/// listed.
pub(crate) fn entry(path: &Path, name: String) -> Entry {
    let kind = match std::fs::symlink_metadata(path.join(".git")) {
        Ok(metadata) if metadata.is_dir() => Kind::Repository(Vec::new()),
        Ok(_) => Kind::LinkedCheckout,
        Err(error) if error.kind() == ErrorKind::NotFound => folder(path),
        Err(error) => Kind::Unreadable(error.kind().to_string()),
    };
    Entry { name, path: path.to_path_buf(), kind }
}

fn folder(path: &Path) -> Kind {
    match folders_in(path) {
        Err(error) => Kind::Unreadable(error.kind().to_string()),
        Ok(children) if children.iter().all(|child| child.kind == Kind::NoRepositories) => Kind::NoRepositories,
        Ok(children) => Kind::Folder(children),
    }
}

fn folders_in(path: &Path) -> std::io::Result<Vec<Entry>> {
    let mut folders = Vec::new();
    for found in std::fs::read_dir(path)? {
        let found = found?;
        let name = found.file_name();
        if found.file_type()?.is_dir() && !name.as_bytes().starts_with(b".") {
            folders.push((name, found.path()));
        }
    }
    folders.sort();
    Ok(folders.into_iter().map(|(name, path)| entry(&path, name.to_string_lossy().into_owned())).collect())
}


// ============
// === Note ===
// ============

/// A remark printed after a repository's name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Note {
    pub(crate) text: String,
    pub(crate) tone: Tone,
}

/// What a note asks of the user, which sets its color.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Tone {
    /// Context only (dim).
    Quiet,
    /// Coming in: the next pull brings it (cyan).
    Incoming,
    /// Exists only on this machine, until it is pushed or for good (yellow).
    OnlyHere,
    /// Needs the user (red).
    Problem,
}

impl Tone {
    fn color(self) -> &'static str {
        match self {
            Self::Quiet => "2",
            Self::Incoming => "36",
            Self::OnlyHere => "33",
            Self::Problem => "31",
        }
    }
}

/// What `list` says about a repository, as of its last fetch: nothing when it is clean and in sync.
pub(crate) fn notes(status: &git::RepoStatus, origin: &git::Origin) -> Vec<Note> {
    let tree = status.working_tree;
    let notes = [
        (*origin == git::Origin::Missing).then(|| note("no origin", Tone::OnlyHere)),
        status.operation.map(|operation| note(&format!("{operation} in progress"), Tone::Problem)),
        head_note(status, origin),
        tree.unmerged.then(|| note("conflicts", Tone::Problem)),
        tree.tracked_changes.then(|| note("modified", Tone::OnlyHere)),
        tree.untracked.then(|| note("untracked files", Tone::Quiet)),
        status.has_stash.then(|| note("stash", Tone::Quiet)),
    ];
    notes.into_iter().flatten().collect()
}

/// The current branch against its upstream, or where HEAD is when it isn't on a branch with commits.
fn head_note(status: &git::RepoStatus, origin: &git::Origin) -> Option<Note> {
    match &status.head {
        git::Head::Detached(_) => Some(note("detached", Tone::OnlyHere)),
        git::Head::Unborn(branch) => Some(note(&format!("{branch}: no commits yet"), Tone::Quiet)),
        git::Head::Branch(branch) => {
            let upstream = status.current_branch().and_then(|info| info.upstream.as_ref());
            match upstream.map(|upstream| upstream.track) {
                None if *origin == git::Origin::Missing => None,
                None => Some(note(&format!("{branch}: no upstream"), Tone::OnlyHere)),
                Some(git::Track::InSync) => None,
                Some(git::Track::Ahead(count)) => Some(note(&format!("{branch} ↑{count}"), Tone::OnlyHere)),
                Some(git::Track::Behind(count)) => Some(note(&format!("{branch} ↓{count}"), Tone::Incoming)),
                Some(git::Track::Diverged { ahead, behind }) => {
                    Some(note(&format!("{branch} ↑{ahead} ↓{behind}"), Tone::Problem))
                }
                Some(git::Track::Gone) => Some(note(&format!("{branch}: upstream gone"), Tone::Problem)),
            }
        }
    }
}

fn note(text: &str, tone: Tone) -> Note {
    Note { text: text.to_owned(), tone }
}


// ==============
// === render ===
// ==============

/// The tree as text, drawn like `tree`: the root, then one line per entry. Folders end in `/`. One without repositories
/// inside is red when `colored`, and says so otherwise.
pub(crate) fn render(root: &Entry, colored: bool) -> String {
    let lines = std::iter::once(label(root, colored)).chain(lines_below(root, "", colored));
    lines.map(|line| format!("{line}\n")).collect()
}

fn lines_below(entry: &Entry, prefix: &str, colored: bool) -> Vec<String> {
    match &entry.kind {
        Kind::Folder(children) => match children.split_last() {
            None => Vec::new(),
            Some((last, others)) => {
                let marked = others.iter().map(|child| (child, false)).chain(std::iter::once((last, true)));
                marked
                    .flat_map(|(child, is_last)| {
                        let (branch, indent) = if is_last { ("└── ", "    ") } else { ("├── ", "│   ") };
                        let line = format!("{prefix}{branch}{}", label(child, colored));
                        std::iter::once(line).chain(lines_below(child, &format!("{prefix}{indent}"), colored))
                    })
                    .collect()
            }
        },
        Kind::Repository(_) | Kind::LinkedCheckout | Kind::NoRepositories | Kind::Unreadable(_) => Vec::new(),
    }
}

fn label(entry: &Entry, colored: bool) -> String {
    let name = &entry.name;
    match &entry.kind {
        Kind::Repository(notes) if notes.is_empty() => name.clone(),
        Kind::Repository(notes) => {
            let painted = notes.iter().map(|note| paint(note.tone.color(), &note.text, colored)).collect::<Vec<_>>();
            format!("{name}  {}", painted.join(" · "))
        }
        Kind::Folder(_) => format!("{name}/"),
        Kind::LinkedCheckout => format!("{name} {}", paint("2", "(linked worktree, not synced)", colored)),
        Kind::NoRepositories => match colored {
            true => paint("31", &format!("{name}/"), true),
            false => format!("{name}/ (no repositories)"),
        },
        Kind::Unreadable(reason) => format!("{} (can't read: {reason})", paint("31", &format!("{name}/"), colored)),
    }
}

/// `text` in the terminal color `code` when `colored`.
fn paint(code: &str, text: &str, colored: bool) -> String {
    match colored {
        true => format!("\u{1b}[{code}m{text}\u{1b}[0m"),
        false => text.to_owned(),
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt as _;

    use crate::domain;
    use crate::fixtures;
    use crate::git;
    use super::Note;
    use super::Tone;
    use super::entry;
    use super::notes;
    use super::render;

    fn note(text: &str, tone: Tone) -> Note {
        Note { text: text.to_owned(), tone }
    }

    /// A repository on `main`, whose upstream is `origin/main` in the state `track` when there is one.
    fn on_main(track: Option<git::Track>) -> anyhow::Result<git::RepoStatus> {
        let name: domain::BranchName = "main".parse()?;
        let upstream = track.map(|track| git::Upstream {
            full_ref: "refs/remotes/origin/main".to_owned(),
            remote: git::UpstreamRemote::Named(domain::RemoteName::origin()),
            remote_ref: "refs/heads/main".to_owned(),
            track,
        });
        Ok(git::RepoStatus {
            head: git::Head::Branch(name.clone()),
            working_tree: git::WorkingTree::default(),
            operation: None,
            has_stash: false,
            branches: vec![git::BranchInfo { name, upstream }],
        })
    }

    #[test]
    fn notes_tell_what_is_only_here_coming_in_or_needing_the_user() -> anyhow::Result<()> {
        let origin = git::Origin::Url(fixtures::url("git@github.com:o/r.git")?);
        let (only_here, incoming, problem) = (Tone::OnlyHere, Tone::Incoming, Tone::Problem);
        assert_eq!(notes(&on_main(Some(git::Track::InSync))?, &origin), []);
        let behind_and_edited = git::RepoStatus {
            working_tree: git::WorkingTree { tracked_changes: true, ..git::WorkingTree::default() },
            ..on_main(Some(git::Track::Behind(22)))?
        };
        assert_eq!(notes(&behind_and_edited, &origin), [note("main ↓22", incoming), note("modified", only_here)]);
        assert_eq!(notes(&on_main(Some(git::Track::Ahead(3)))?, &origin), [note("main ↑3", only_here)]);
        let diverged = on_main(Some(git::Track::Diverged { ahead: 1, behind: 2 }))?;
        assert_eq!(notes(&diverged, &origin), [note("main ↑1 ↓2", problem)]);
        assert_eq!(notes(&on_main(Some(git::Track::Gone))?, &origin), [note("main: upstream gone", problem)]);
        assert_eq!(notes(&on_main(None)?, &origin), [note("main: no upstream", only_here)]);
        assert_eq!(notes(&on_main(None)?, &git::Origin::Missing), [note("no origin", only_here)]);
        let detached = git::RepoStatus { head: git::Head::Detached("a".repeat(40).parse()?), ..on_main(None)? };
        assert_eq!(notes(&detached, &origin), [note("detached", only_here)]);
        let unborn = git::RepoStatus { head: git::Head::Unborn("main".parse()?), branches: vec![], ..on_main(None)? };
        assert_eq!(notes(&unborn, &origin), [note("main: no commits yet", Tone::Quiet)]);
        let merging = git::RepoStatus {
            operation: Some(git::Operation::Merge),
            working_tree: git::WorkingTree { unmerged: true, untracked: true, ..git::WorkingTree::default() },
            has_stash: true,
            ..on_main(Some(git::Track::InSync))?
        };
        let expected = [
            note("merge in progress", problem),
            note("conflicts", problem),
            note("untracked files", Tone::Quiet),
            note("stash", Tone::Quiet),
        ];
        assert_eq!(notes(&merging, &origin), expected);
        Ok(())
    }

    #[test]
    fn notes_follow_their_repository_in_color() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        for repository in ["a", "b"] {
            std::fs::create_dir_all(dir.path().join(repository).join(".git"))?;
        }
        let tree = entry(dir.path(), "dev".to_owned());
        assert_eq!(tree.repositories(), [dir.path().join("a"), dir.path().join("b")]);
        let written = vec![note("main ↓2", Tone::Incoming), note("modified", Tone::OnlyHere)];
        let tree = tree.with_notes(&mut BTreeMap::from([(dir.path().join("a"), written)]));
        assert_eq!(render(&tree, false), "dev/\n├── a  main ↓2 · modified\n└── b\n");
        let colored = "dev/\n├── a  \u{1b}[36mmain ↓2\u{1b}[0m · \u{1b}[33mmodified\u{1b}[0m\n└── b\n";
        assert_eq!(render(&tree, true), colored);
        Ok(())
    }

    #[test]
    fn draws_folders_down_to_repositories_and_flags_folders_without_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path();
        for repository in ["a", "b/c", "x", "x/inner", ".hidden/g"] {
            std::fs::create_dir_all(root.join(repository).join(".git"))?;
        }
        std::fs::create_dir_all(root.join("b").join("notes"))?;
        std::fs::write(root.join("b").join("notes").join("todo.txt"), "not synced")?;
        std::fs::create_dir_all(root.join("d"))?;
        std::fs::create_dir_all(root.join("e").join("f"))?;
        std::fs::create_dir_all(root.join("w"))?;
        std::fs::write(root.join("w").join(".git"), "gitdir: /elsewhere\n")?;
        std::os::unix::fs::symlink(root.join("a"), root.join("link"))?;
        std::fs::write(root.join("file.txt"), "")?;
        assert_eq!(
            render(&entry(root, "dev".to_owned()), false),
            "dev/\n\
             ├── a\n\
             ├── b/\n\
             │   ├── c\n\
             │   └── notes/ (no repositories)\n\
             ├── d/ (no repositories)\n\
             ├── e/ (no repositories)\n\
             ├── w (linked worktree, not synced)\n\
             └── x\n"
        );
        Ok(())
    }

    #[test]
    fn a_folder_without_repositories_is_red_when_colored() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("a").join(".git"))?;
        std::fs::create_dir_all(dir.path().join("notes"))?;
        let tree = entry(dir.path(), "dev".to_owned());
        assert_eq!(render(&tree, true), "dev/\n├── a\n└── \u{1b}[31mnotes/\u{1b}[0m\n");
        Ok(())
    }

    #[test]
    fn the_root_can_hold_no_repository_or_be_one() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert_eq!(render(&entry(dir.path(), "dev".to_owned()), false), "dev/ (no repositories)\n");
        std::fs::create_dir_all(dir.path().join("sub").join(".git"))?;
        std::fs::create_dir(dir.path().join(".git"))?;
        assert_eq!(render(&entry(dir.path(), "dev".to_owned()), false), "dev\n");
        Ok(())
    }

    #[test]
    fn an_unreadable_folder_is_shown_with_the_reason() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::create_dir_all(dir.path().join("a").join(".git"))?;
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked)?;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))?;
        let privileged = std::fs::read_dir(&locked).is_ok();
        let rendered = render(&entry(dir.path(), "dev".to_owned()), false);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))?;
        if !privileged {
            assert_eq!(rendered, "dev/\n├── a\n└── locked/ (can't read: permission denied)\n");
        }
        Ok(())
    }
}
