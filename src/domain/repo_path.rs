use std::fmt;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::str::FromStr;

use anyhow::Context as _;


// ================
// === RepoPath ===
// ================

/// A repository's location relative to the workspace root, such as `ferrisoft/website`.
///
/// Components are separated by `/`. Each is non-empty, not `.` or `..`, not hidden (a leading `.` is reserved for
/// tooling such as `.git`, `.dev_sync` and temporary directories) and free of control characters. Invalid input is
/// rejected, never normalized.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct RepoPath {
    path: String,
}

impl RepoPath {
    /// Strips `root` from a filesystem path below it. Non-UTF-8 names are an error.
    pub(crate) fn from_fs_path(root: &Path, path: &Path) -> anyhow::Result<Self> {
        let relative = path
            .strip_prefix(root)
            .map_err(|_| anyhow::anyhow!("{} is not inside {}", path.display(), root.display()))?;
        let components = relative
            .components()
            .map(|component| match component {
                Component::Normal(name) => name
                    .to_str()
                    .with_context(|| format!("{} has a name that is not valid UTF-8", path.display())),
                Component::Prefix(_) | Component::RootDir | Component::CurDir | Component::ParentDir => {
                    Err(anyhow::anyhow!("{} is not a plain path below {}", path.display(), root.display()))
                }
            })
            .collect::<anyhow::Result<Vec<&str>>>()?;
        components.join("/").parse()
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.path
    }

    pub(crate) fn components(&self) -> impl Iterator<Item = &str> {
        self.path.split('/')
    }

    /// The last component.
    pub(crate) fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    pub(crate) fn to_fs_path(&self, root: &Path) -> PathBuf {
        self.components().fold(root.to_path_buf(), |path, component| path.join(component))
    }

    /// True when the paths are equal or one is an ancestor of the other.
    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        self == other || self.is_proper_ancestor_of(other) || other.is_proper_ancestor_of(self)
    }

    pub(crate) fn is_proper_ancestor_of(&self, other: &Self) -> bool {
        other.path.strip_prefix(&self.path).is_some_and(|rest| rest.starts_with('/'))
    }

    /// `a/b/c` → `[a, a/b]`.
    pub(crate) fn proper_ancestors(&self) -> Vec<Self> {
        self.path
            .match_indices('/')
            .filter_map(|(index, _)| self.path.get(..index))
            .map(|path| Self { path: path.to_owned() })
            .collect()
    }
}

impl FromStr for RepoPath {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> anyhow::Result<Self> {
        validate(text)
            .map(|()| Self { path: text.to_owned() })
            .with_context(|| format!("invalid repository path {text:?}"))
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path)
    }
}

fn validate(text: &str) -> anyhow::Result<()> {
    match text {
        "" => Err(anyhow::anyhow!("it is empty")),
        _ => text.split('/').try_for_each(validate_component),
    }
}

fn validate_component(component: &str) -> anyhow::Result<()> {
    match component {
        "" => Err(anyhow::anyhow!("it has an empty component (a leading, trailing or doubled `/`)")),
        "." | ".." => Err(anyhow::anyhow!("`.` and `..` are not allowed")),
        _ if component.starts_with('.') => {
            Err(anyhow::anyhow!("`{component}` starts with `.`; hidden names are reserved for tooling"))
        }
        _ if component.chars().any(char::is_control) => Err(anyhow::anyhow!("it contains a control character")),
        _ => Ok(()),
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::Path;
    use std::path::PathBuf;

    use super::RepoPath;

    fn path(text: &str) -> anyhow::Result<RepoPath> {
        text.parse()
    }

    #[test]
    fn accepts_valid_paths() -> anyhow::Result<()> {
        for text in ["a", "b/c", "ferrisoft/design_system", "zażółć/x", "my repo/x", "a\"b", "a\\b"] {
            assert_eq!(path(text)?.as_str(), text);
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_paths() {
        for text in ["", "/a", "a/", "a//b", ".", "..", "a/../b", "a/./b", ".hidden", "a/.git", "a\nb", "a\0b"] {
            assert!(path(text).is_err(), "{text:?} should be rejected");
        }
    }

    #[test]
    fn overlaps_only_ancestors_and_equal_paths() -> anyhow::Result<()> {
        assert!(path("a")?.overlaps(&path("a/b")?));
        assert!(path("a/b")?.overlaps(&path("a")?));
        assert!(!path("a")?.overlaps(&path("ab")?));
        assert!(path("a/b")?.overlaps(&path("a/b")?));
        assert!(!path("a-b")?.overlaps(&path("a/b")?));
        Ok(())
    }

    #[test]
    fn lists_proper_ancestors() -> anyhow::Result<()> {
        assert_eq!(path("a/b/c")?.proper_ancestors(), vec![path("a")?, path("a/b")?]);
        assert_eq!(path("a")?.proper_ancestors(), vec![]);
        Ok(())
    }

    #[test]
    fn converts_to_and_from_filesystem_paths() -> anyhow::Result<()> {
        let root = Path::new("/w");
        let repo = path("zażółć/my repo")?;
        assert_eq!(repo.to_fs_path(root), PathBuf::from("/w/zażółć/my repo"));
        assert_eq!(RepoPath::from_fs_path(root, &repo.to_fs_path(root))?, repo);
        let outside = RepoPath::from_fs_path(root, Path::new("/elsewhere/x")).err().map(|error| format!("{error:#}"));
        assert_eq!(outside.as_deref(), Some("/elsewhere/x is not inside /w"));
        assert!(RepoPath::from_fs_path(root, root).is_err());
        Ok(())
    }

    #[test]
    fn rejects_non_utf8_filesystem_names() {
        let name = std::ffi::OsStr::from_bytes(b"bad\xffname");
        assert!(RepoPath::from_fs_path(Path::new("/w"), &Path::new("/w").join(name)).is_err());
    }

    #[test]
    fn orders_by_bytes() -> anyhow::Result<()> {
        let mut paths = vec![path("a/b")?, path("a-b")?, path("a")?];
        paths.sort();
        assert_eq!(paths, vec![path("a")?, path("a-b")?, path("a/b")?]);
        Ok(())
    }

    #[test]
    fn names_the_last_component() -> anyhow::Result<()> {
        assert_eq!(path("a/b/c")?.name(), "c");
        assert_eq!(path("a")?.name(), "a");
        Ok(())
    }
}
