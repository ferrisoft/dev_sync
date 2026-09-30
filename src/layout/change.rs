use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use crate::domain;
use crate::layout::model;


// ==============
// === Change ===
// ==============

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Change {
    Add { path: domain::RepoPath, url: domain::RemoteUrl },
    Remove { path: domain::RepoPath, url: domain::RemoteUrl },
    Move { from: domain::RepoPath, to: domain::RepoPath, url: domain::RemoteUrl },
    SetUrl { path: domain::RepoPath, from: domain::RemoteUrl, to: domain::RemoteUrl },
}

impl Change {
    /// One path, or two for a move (source first).
    pub(crate) fn paths(&self) -> Vec<&domain::RepoPath> {
        match self {
            Self::Add { path, .. } | Self::Remove { path, .. } | Self::SetUrl { path, .. } => vec![path],
            Self::Move { from, to, .. } => vec![from, to],
        }
    }

    pub(crate) fn first_path(&self) -> &domain::RepoPath {
        match self {
            Self::Add { path, .. } | Self::Remove { path, .. } | Self::SetUrl { path, .. } => path,
            Self::Move { from, .. } => from,
        }
    }
}

/// `+path`, `-path`, `from → to`, or `path: <old url> → <new url>`.
impl fmt::Display for Change {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Add { path, .. } => write!(f, "+{path}"),
            Self::Remove { path, .. } => write!(f, "-{path}"),
            Self::Move { from, to, .. } => write!(f, "{from} → {to}"),
            Self::SetUrl { path, from, to } => write!(f, "{path}: {from} → {to}"),
        }
    }
}


// ============
// === diff ===
// ============

/// The changes that turn `old` into `new`. A removal and an addition of the same URL pair up into a move; when several
/// clones share a URL, sorted paths pair in order, which is arbitrary but deterministic. Sorted by each change's first
/// path.
pub(crate) fn diff(old: &model::Layout, new: &model::Layout) -> Vec<Change> {
    let set_urls = old.repos().filter_map(|repo| {
        new.get(&repo.path)
            .filter(|entry| entry.url != repo.url)
            .map(|entry| Change::SetUrl { path: repo.path.clone(), from: repo.url.clone(), to: entry.url.clone() })
    });
    let removed = group_by_url(old.repos().filter(|repo| new.get(&repo.path).is_none()));
    let added = group_by_url(new.repos().filter(|repo| old.get(&repo.path).is_none()));
    let urls = removed.keys().chain(added.keys()).collect::<BTreeSet<_>>();
    let paired = urls.into_iter().flat_map(|url| {
        let gone = removed.get(url).map(Vec::as_slice).unwrap_or_default();
        let came = added.get(url).map(Vec::as_slice).unwrap_or_default();
        pair_up(url, gone, came)
    });
    let mut changes = set_urls.chain(paired).collect::<Vec<_>>();
    changes.sort_by(|left, right| left.first_path().cmp(right.first_path()));
    changes
}

fn group_by_url(repos: impl Iterator<Item = model::LayoutRepo>) -> BTreeMap<domain::RemoteUrl, Vec<domain::RepoPath>> {
    repos.fold(BTreeMap::new(), |mut groups, repo| {
        groups.entry(repo.url).or_insert_with(Vec::new).push(repo.path);
        groups
    })
}

fn pair_up(url: &domain::RemoteUrl, removed: &[domain::RepoPath], added: &[domain::RepoPath]) -> Vec<Change> {
    let moves = removed.iter().zip(added).map(|(from, to)| Change::Move {
        from: from.clone(),
        to: to.clone(),
        url: url.clone(),
    });
    let paired = removed.len().min(added.len());
    let removals = removed.iter().skip(paired).map(|path| Change::Remove { path: path.clone(), url: url.clone() });
    let additions = added.iter().skip(paired).map(|path| Change::Add { path: path.clone(), url: url.clone() });
    moves.chain(removals).chain(additions).collect()
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use crate::fixtures;
    use super::diff;

    #[test]
    fn diff_finds_adds_removes_and_url_changes() -> anyhow::Result<()> {
        let old = fixtures::layout("a=u1 b=u2")?;
        let new = fixtures::layout("b=u3 x=u2")?;
        assert_eq!(diff(&old, &new), vec![
            fixtures::remove("a", "u1")?,
            fixtures::set_url("b", "u2", "u3")?,
            fixtures::add("x", "u2")?,
        ]);
        Ok(())
    }

    #[test]
    fn diff_pairs_a_removal_and_an_addition_of_one_url_into_a_move() -> anyhow::Result<()> {
        assert_eq!(diff(&fixtures::layout("a=u1")?, &fixtures::layout("x=u1")?), vec![fixtures::move_to(
            "a", "x", "u1"
        )?]);
        Ok(())
    }

    #[test]
    fn diff_moves_only_the_clone_that_left() -> anyhow::Result<()> {
        let old = fixtures::layout("a=u1 b=u1")?;
        let new = fixtures::layout("a=u1 x=u1")?;
        assert_eq!(diff(&old, &new), vec![fixtures::move_to("b", "x", "u1")?]);
        Ok(())
    }

    #[test]
    fn diff_is_deterministic() -> anyhow::Result<()> {
        let old = fixtures::layout("a=u1 b=u1 c=u2 d=u3")?;
        let new = fixtures::layout("x=u1 y=u1 c=u3 z=u2")?;
        let first = diff(&old, &new);
        assert!((0..20).all(|_| diff(&old, &new) == first));
        assert_eq!(first, vec![
            fixtures::move_to("a", "x", "u1")?,
            fixtures::move_to("b", "y", "u1")?,
            fixtures::set_url("c", "u2", "u3")?,
            fixtures::remove("d", "u3")?,
            fixtures::add("z", "u2")?,
        ]);
        Ok(())
    }

    #[test]
    fn diff_of_equal_layouts_is_empty() -> anyhow::Result<()> {
        let layout = fixtures::layout("a=u1 b/c=u2")?;
        assert_eq!(diff(&layout, &layout), vec![]);
        Ok(())
    }

    #[test]
    fn lists_paths_and_displays_compactly() -> anyhow::Result<()> {
        let moved = fixtures::move_to("a", "b/c", "u1")?;
        assert_eq!(moved.paths(), vec![&fixtures::path("a")?, &fixtures::path("b/c")?]);
        assert_eq!(fixtures::add("a", "u1")?.to_string(), "+a");
        assert_eq!(fixtures::remove("a", "u1")?.to_string(), "-a");
        assert_eq!(moved.to_string(), "a → b/c");
        assert_eq!(fixtures::set_url("a", "u1", "u2")?.to_string(), "a: u1 → u2");
        Ok(())
    }
}
