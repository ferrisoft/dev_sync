use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use crate::domain;
use crate::layout::change;


// =============
// === Entry ===
// =============

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) url: domain::RemoteUrl,
}


// ==================
// === LayoutRepo ===
// ==================

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct LayoutRepo {
    pub(crate) path: domain::RepoPath,
    pub(crate) url: domain::RemoteUrl,
}


// ==============
// === Layout ===
// ==============

/// The repositories that make up the workspace, keyed by location. No location is nested inside another.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Layout {
    repos: BTreeMap<domain::RepoPath, Entry>,
}

impl Layout {
    /// Rejects a path listed twice and a path nested inside another, naming every problem.
    pub(crate) fn from_repos(repos: Vec<LayoutRepo>) -> anyhow::Result<Self> {
        let mut map = BTreeMap::new();
        let mut problems = Vec::new();
        for LayoutRepo { path, url } in repos {
            if map.insert(path.clone(), Entry { url }).is_some() {
                problems.push(format!("`{path}` is listed more than once"));
            }
        }
        let layout = Self { repos: map };
        let nested = layout
            .repos
            .keys()
            .filter_map(|path| layout.nesting_parent(path).map(|parent| format!("`{path}` is inside `{parent}`")));
        let problems = problems.into_iter().chain(nested).collect::<Vec<_>>();
        match problems.is_empty() {
            true => Ok(layout),
            false => Err(anyhow::anyhow!("{}", problems.join("; "))),
        }
    }

    pub(crate) fn get(&self, path: &domain::RepoPath) -> Option<&Entry> {
        self.repos.get(path)
    }

    /// Sorted by path.
    pub(crate) fn repos(&self) -> impl Iterator<Item = LayoutRepo> + '_ {
        self.repos.iter().map(|(path, entry)| LayoutRepo { path: path.clone(), url: entry.url.clone() })
    }

    /// Applies every change as one batch, so swaps and chains work: first every removal and move source leaves, then
    /// every url changes, then every addition and move destination arrives, and finally no path may be nested inside
    /// another. Any failed precondition rejects the whole batch, listing every problem.
    pub(crate) fn apply(&self, changes: &[change::Change]) -> Applied {
        let mut repos = self.repos.clone();
        let mut rejections = Vec::new();
        for change in changes {
            let problem = match change {
                change::Change::Remove { path, url } | change::Change::Move { from: path, url, .. } => {
                    match repos.get(path) {
                        None => Some(RejectionReason::PathAbsent),
                        Some(entry) if entry.url != *url => Some(RejectionReason::UrlMismatch),
                        Some(_) => {
                            repos.remove(path);
                            None
                        }
                    }
                }
                change::Change::Add { .. } | change::Change::SetUrl { .. } => None,
            };
            rejections.extend(problem.map(|reason| ChangeRejection { change: change.clone(), reason }));
        }
        for change in changes {
            let problem = match change {
                change::Change::SetUrl { path, from, to } => match repos.get_mut(path) {
                    None => Some(RejectionReason::PathAbsent),
                    Some(entry) if entry.url != *from => Some(RejectionReason::UrlMismatch),
                    Some(entry) => {
                        entry.url = to.clone();
                        None
                    }
                },
                change::Change::Add { .. } | change::Change::Remove { .. } | change::Change::Move { .. } => None,
            };
            rejections.extend(problem.map(|reason| ChangeRejection { change: change.clone(), reason }));
        }
        let mut arrived = Vec::new();
        for change in changes {
            let problem = match change {
                change::Change::Add { path, url } | change::Change::Move { to: path, url, .. } => {
                    match repos.contains_key(path) {
                        true => Some(RejectionReason::PathPresent),
                        false => {
                            repos.insert(path.clone(), Entry { url: url.clone() });
                            arrived.push(ArrivedChange { change, path });
                            None
                        }
                    }
                }
                change::Change::Remove { .. } | change::Change::SetUrl { .. } => None,
            };
            rejections.extend(problem.map(|reason| ChangeRejection { change: change.clone(), reason }));
        }
        let result = Self { repos };
        let nesting = arrived.iter().filter_map(|arrived| {
            result.overlapping(arrived.path).map(|with| ChangeRejection {
                change: arrived.change.clone(),
                reason: RejectionReason::Nested { with: with.clone() },
            })
        });
        rejections.extend(nesting);
        match rejections.is_empty() {
            true => Applied::Ok(result),
            false => Applied::Rejected(rejections),
        }
    }

    /// The layout without the given paths. Removing entries can't break an invariant, so this can't fail.
    pub(crate) fn without(&self, paths: &BTreeSet<domain::RepoPath>) -> Self {
        let repos = self.repos.iter().filter(|(path, _)| !paths.contains(path));
        Self { repos: repos.map(|(path, entry)| (path.clone(), entry.clone())).collect() }
    }

    /// The nearest proper ancestor of `path` that is in the layout.
    fn nesting_parent(&self, path: &domain::RepoPath) -> Option<domain::RepoPath> {
        path.proper_ancestors().into_iter().rev().find(|ancestor| self.repos.contains_key(ancestor))
    }

    /// Another path in the layout that is an ancestor or a descendant of `path`.
    fn overlapping(&self, path: &domain::RepoPath) -> Option<&domain::RepoPath> {
        self.repos.keys().find(|other| *other != path && other.overlaps(path))
    }
}

struct ArrivedChange<'a> {
    change: &'a change::Change,
    path: &'a domain::RepoPath,
}


// ===============
// === Applied ===
// ===============

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum Applied {
    Ok(Layout),
    Rejected(Vec<ChangeRejection>),
}


// =======================
// === ChangeRejection ===
// =======================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ChangeRejection {
    pub(crate) change: change::Change,
    pub(crate) reason: RejectionReason,
}


// =======================
// === RejectionReason ===
// =======================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum RejectionReason {
    PathAbsent,
    PathPresent,
    UrlMismatch,
    Nested { with: domain::RepoPath },
}

impl fmt::Display for RejectionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PathAbsent => f.write_str("the layout has nothing there"),
            Self::PathPresent => f.write_str("the layout already has a repository there"),
            Self::UrlMismatch => f.write_str("the layout has another URL there"),
            Self::Nested { with } => write!(f, "it would be nested with {with}"),
        }
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use crate::fixtures;
    use crate::layout::change::Change;
    use super::Applied;
    use super::ChangeRejection;
    use super::RejectionReason;

    fn rejection(change: anyhow::Result<Change>, reason: RejectionReason) -> anyhow::Result<ChangeRejection> {
        Ok(ChangeRejection { change: change?, reason })
    }

    #[test]
    fn from_repos_rejects_duplicates_and_nesting_but_accepts_siblings() -> anyhow::Result<()> {
        assert!(fixtures::layout("a=u1 a=u2").is_err());
        assert!(fixtures::layout("a=u1 a/b=u2").is_err());
        assert!(fixtures::layout("a/b/c=u1 a=u2").is_err());
        assert_eq!(fixtures::layout("a=u1 ab=u2 a-b=u3 b/a=u4 b/c=u5")?.repos().count(), 5);
        Ok(())
    }

    #[test]
    fn apply_swaps_in_one_batch() -> anyhow::Result<()> {
        let layout = fixtures::layout("a=u1 b=u2")?;
        let swap = [fixtures::move_to("a", "b", "u1")?, fixtures::move_to("b", "a", "u2")?];
        assert_eq!(layout.apply(&swap), Applied::Ok(fixtures::layout("a=u2 b=u1")?));
        Ok(())
    }

    #[test]
    fn apply_follows_a_chain() -> anyhow::Result<()> {
        let layout = fixtures::layout("a=u1 b=u2")?;
        let chain = [fixtures::move_to("a", "b", "u1")?, fixtures::move_to("b", "c", "u2")?];
        assert_eq!(layout.apply(&chain), Applied::Ok(fixtures::layout("b=u1 c=u2")?));
        Ok(())
    }

    #[test]
    fn apply_adds_removes_and_sets_urls() -> anyhow::Result<()> {
        let layout = fixtures::layout("a=u1 b=u2")?;
        let changes = [fixtures::add("c", "u3")?, fixtures::remove("a", "u1")?, fixtures::set_url("b", "u2", "u4")?];
        assert_eq!(layout.apply(&changes), Applied::Ok(fixtures::layout("b=u4 c=u3")?));
        Ok(())
    }

    #[test]
    fn apply_rejects_each_failed_precondition() -> anyhow::Result<()> {
        let layout = fixtures::layout("a=u1 b=u2")?;
        let cases = [
            (fixtures::remove("x", "u1"), RejectionReason::PathAbsent),
            (fixtures::remove("a", "u2"), RejectionReason::UrlMismatch),
            (fixtures::move_to("x", "y", "u1"), RejectionReason::PathAbsent),
            (fixtures::move_to("a", "b", "u1"), RejectionReason::PathPresent),
            (fixtures::add("a", "u3"), RejectionReason::PathPresent),
            (fixtures::set_url("x", "u1", "u2"), RejectionReason::PathAbsent),
            (fixtures::set_url("a", "u2", "u3"), RejectionReason::UrlMismatch),
            (fixtures::add("a/c", "u3"), RejectionReason::Nested { with: fixtures::path("a")? }),
            (fixtures::move_to("a", "b/x", "u1"), RejectionReason::Nested { with: fixtures::path("b")? }),
        ];
        for (change, reason) in cases {
            let change = change?;
            let expected = Applied::Rejected(vec![rejection(Ok(change.clone()), reason)?]);
            assert_eq!(layout.apply(std::slice::from_ref(&change)), expected, "{change:?}");
        }
        Ok(())
    }

    #[test]
    fn apply_lists_every_problem() -> anyhow::Result<()> {
        let layout = fixtures::layout("a=u1")?;
        let changes = [fixtures::remove("x", "u1")?, fixtures::add("a", "u2")?];
        match layout.apply(&changes) {
            Applied::Rejected(rejections) => assert_eq!(rejections.len(), 2),
            Applied::Ok(result) => anyhow::bail!("expected a rejection, got {result:?}"),
        }
        Ok(())
    }
}
