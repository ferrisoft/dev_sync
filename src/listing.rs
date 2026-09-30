//! The tree `list` prints: a dev folder's folders down to its repositories.

use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt as _;
use std::path::Path;


// =============
// === Entry ===
// =============

/// A folder in the tree, named as it appears under its parent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) name: String,
    pub(crate) kind: Kind,
}

/// What a folder turned out to be.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    Repository,
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
        Ok(metadata) if metadata.is_dir() => Kind::Repository,
        Ok(_) => Kind::LinkedCheckout,
        Err(error) if error.kind() == ErrorKind::NotFound => folder(path),
        Err(error) => Kind::Unreadable(error.kind().to_string()),
    };
    Entry { name, kind }
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
        Kind::Repository | Kind::LinkedCheckout | Kind::NoRepositories | Kind::Unreadable(_) => Vec::new(),
    }
}

fn label(entry: &Entry, colored: bool) -> String {
    let name = &entry.name;
    match &entry.kind {
        Kind::Repository => name.clone(),
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
    use std::os::unix::fs::PermissionsExt as _;

    use super::entry;
    use super::render;

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
