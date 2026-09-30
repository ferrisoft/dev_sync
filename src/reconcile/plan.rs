//! What a pull must do on disk to make it match the layout (§8.7). Pure.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;

use crate::domain;
use crate::git;
use crate::layout;
use crate::reconcile::facts;
use crate::scan;
use crate::state;


// ==============
// === Action ===
// ==============

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Action {
    /// Already on disk with the right URL; just record it.
    Adopt { path: domain::RepoPath },
    /// Checked for local-only work when it runs.
    Remove { path: domain::RepoPath },
    Move { from: domain::RepoPath, to: domain::RepoPath },
    SetUrl { path: domain::RepoPath, url: domain::RemoteUrl },
    Clone { path: domain::RepoPath, url: domain::RemoteUrl },
}


// ============
// === Plan ===
// ============

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[must_use]
pub(crate) struct Plan {
    /// Adoptions, removals, moves, url changes, then clones; each sorted by path.
    pub(crate) actions: Vec<Action>,
    pub(crate) conflicts: Vec<DiskConflict>,
}

/// Compares what this machine had (`base`) with what the layout wants (`target`). Leaving and arriving entries of
/// one URL pair up into moves, which covers plain moves, chains and swaps; what's left becomes url changes (same
/// path), removals and clones. A destination may be where repositories are leaving from (see `landing`).
pub(crate) fn plan(
    base: &state::MachineState,
    target: &layout::Layout,
    observed: &[scan::ObservedRepo],
    facts: &facts::DiskFacts,
) -> Plan {
    let on_disk = observed.iter().map(|repo| (&repo.path, &repo.origin)).collect::<BTreeMap<_, _>>();
    let leaving = base
        .repos
        .iter()
        .filter(|(path, known)| target.get(path).is_none_or(|entry| entry.url != known.url))
        .map(|(path, known)| layout::LayoutRepo { path: path.clone(), url: known.url.clone() })
        .collect::<Vec<_>>();
    let leaving_paths = leaving.iter().map(|repo| repo.path.clone()).collect::<BTreeSet<_>>();
    let mut adopted = BTreeSet::new();
    let mut conflicts = Vec::new();
    let mut arriving = Vec::new();
    let candidates = target.repos().filter(|repo| base.repos.get(&repo.path).is_none_or(|known| known.url != repo.url));
    for repo in candidates {
        match on_disk.get(&repo.path) {
            Some(git::Origin::Url(url)) if *url == repo.url => {
                adopted.insert(repo.path);
            }
            Some(origin) if !leaving_paths.contains(&repo.path) => {
                let blocker = Blocker::OtherRepo { origin: (*origin).clone() };
                conflicts.push(DiskConflict { path: repo.path, from: None, blocker });
            }
            Some(_) | None => arriving.push(repo),
        }
    }
    let leaving = leaving.into_iter().filter(|repo| !adopted.contains(&repo.path)).collect::<Vec<_>>();
    let paired = pair_by_url(leaving, arriving);
    let removals = paired.removals.iter().cloned().collect::<BTreeSet<_>>();
    let sources = paired.moves.iter().map(|relocation| relocation.from.clone());
    let departing = sources.chain(removals.iter().cloned()).collect::<BTreeSet<_>>();
    let landing = landing(paired.moves, paired.clones, &removals, &departing, facts);
    conflicts.extend(landing.conflicts);
    let actions = adopted
        .into_iter()
        .map(|path| Action::Adopt { path })
        .chain(paired.removals.into_iter().map(|path| Action::Remove { path }))
        .chain(landing.moves.into_iter().map(|placement| Action::Move { from: placement.from, to: placement.to }))
        .chain(paired.set_urls.into_iter().map(|repo| Action::SetUrl { path: repo.path, url: repo.url }))
        .chain(landing.clones.into_iter().map(|repo| Action::Clone { path: repo.path, url: repo.url }))
        .collect();
    Plan { actions, conflicts }
}

struct Paired {
    moves: Vec<Relocation>,
    set_urls: Vec<layout::LayoutRepo>,
    removals: Vec<domain::RepoPath>,
    clones: Vec<layout::LayoutRepo>,
}

/// Zips the sorted leaving and arriving paths of each URL into moves; leftovers at one path become url changes, the
/// rest removals and clones.
fn pair_by_url(leaving: Vec<layout::LayoutRepo>, arriving: Vec<layout::LayoutRepo>) -> Paired {
    let group = |repos: Vec<layout::LayoutRepo>| {
        repos.into_iter().fold(BTreeMap::<domain::RemoteUrl, Vec<domain::RepoPath>>::new(), |mut groups, repo| {
            groups.entry(repo.url).or_default().push(repo.path);
            groups
        })
    };
    let mut arriving = group(arriving);
    let mut moves = Vec::new();
    let mut left = BTreeSet::new();
    let mut unmatched = Vec::new();
    for (url, sources) in group(leaving) {
        let destinations = arriving.remove(&url).unwrap_or_default();
        let paired = sources.len().min(destinations.len());
        let relocations = sources.iter().zip(&destinations);
        moves.extend(relocations.map(|(from, to)| Relocation { from: from.clone(), to: to.clone() }));
        left.extend(sources.into_iter().skip(paired));
        let extra = destinations.into_iter().skip(paired);
        unmatched.extend(extra.map(|path| layout::LayoutRepo { path, url: url.clone() }));
    }
    let rest = arriving.into_iter().flat_map(|(url, paths)| {
        paths.into_iter().map(move |path| layout::LayoutRepo { path, url: url.clone() })
    });
    unmatched.extend(rest);
    unmatched.sort();
    moves.sort();
    let (set_urls, clones): (Vec<_>, Vec<_>) = unmatched.into_iter().partition(|repo| left.contains(&repo.path));
    let retargeted = set_urls.iter().map(|repo| &repo.path).collect::<BTreeSet<_>>();
    let removals = left.iter().filter(|path| !retargeted.contains(path)).cloned().collect();
    Paired { moves, set_urls, removals, clones }
}


// ==================
// === Relocation ===
// ==================

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct Relocation {
    pub(crate) from: domain::RepoPath,
    pub(crate) to: domain::RepoPath,
}


// =================
// === Placement ===
// =================

/// A move whose destination can take the repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Placement {
    pub(crate) from: domain::RepoPath,
    pub(crate) to: domain::RepoPath,
    /// The repositories that must be gone before this one can land: sources of moves or removals — this move's own
    /// source too, when the destination is inside it.
    pub(crate) needs: BTreeSet<domain::RepoPath>,
}


// ===============
// === Landing ===
// ===============

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[must_use]
pub(crate) struct Landing {
    pub(crate) moves: Vec<Placement>,
    pub(crate) clones: Vec<layout::LayoutRepo>,
    pub(crate) conflicts: Vec<DiskConflict>,
}

/// Keeps the moves and clones whose destination can take a repository once the repositories at `gone` and the
/// sources of the kept moves have left. Dropping a move keeps its source where it is, which can block other moves in
/// turn, so moves are dropped until every kept one can land. `departing` tells the repositories that were to leave
/// (whose trouble is reported on its own) from those that never were.
pub(crate) fn landing(
    moves: Vec<Relocation>,
    clones: Vec<layout::LayoutRepo>,
    gone: &BTreeSet<domain::RepoPath>,
    departing: &BTreeSet<domain::RepoPath>,
    facts: &facts::DiskFacts,
) -> Landing {
    let mut landing = settle(moves, gone, departing, facts, Vec::new());
    let vacated = gone.iter().chain(landing.moves.iter().map(|placement| &placement.from)).cloned().collect();
    for repo in clones {
        match requirements(facts.destinations.get(&repo.path), &repo.path, &vacated, departing) {
            Ok(_) => landing.clones.push(repo),
            Err(blocker) => landing.conflicts.push(DiskConflict { path: repo.path, from: None, blocker }),
        }
    }
    landing
}

fn settle(
    moves: Vec<Relocation>,
    gone: &BTreeSet<domain::RepoPath>,
    departing: &BTreeSet<domain::RepoPath>,
    facts: &facts::DiskFacts,
    conflicts: Vec<DiskConflict>,
) -> Landing {
    let vacated = gone.iter().chain(moves.iter().map(|relocation| &relocation.from)).cloned().collect();
    let mut placements = Vec::new();
    let mut dropped = Vec::new();
    for Relocation { from, to } in moves {
        match requirements(facts.destinations.get(&to), &to, &vacated, departing) {
            Ok(needs) => placements.push(Placement { from, to, needs }),
            Err(blocker) => dropped.push(DiskConflict { path: to, from: Some(from), blocker }),
        }
    }
    match dropped.is_empty() {
        true => Landing { moves: placements, clones: Vec::new(), conflicts },
        false => {
            let kept = placements.into_iter().map(|placement| Relocation { from: placement.from, to: placement.to });
            let conflicts = conflicts.into_iter().chain(dropped).collect();
            settle(kept.collect(), gone, departing, facts, conflicts)
        }
    }
}

/// What must leave `path` before a repository can land there, or what keeps it from landing once `vacated` has left.
fn requirements(
    fact: Option<&facts::DestinationFact>,
    path: &domain::RepoPath,
    vacated: &BTreeSet<domain::RepoPath>,
    departing: &BTreeSet<domain::RepoPath>,
) -> Result<BTreeSet<domain::RepoPath>, Blocker> {
    let staying = |repo: &domain::RepoPath, otherwise: fn(domain::RepoPath) -> Blocker| {
        match (vacated.contains(repo), departing.contains(repo)) {
            (true, _) => None,
            (false, true) => Some(Blocker::Staying(repo.clone())),
            (false, false) => Some(otherwise(repo.clone())),
        }
    };
    let (needs, blocker) = match fact {
        None | Some(facts::DestinationFact::Free) => (BTreeSet::new(), None),
        Some(facts::DestinationFact::Directory { repos }) => {
            (repos.clone(), repos.iter().find_map(|repo| staying(repo, Blocker::RepoInTheWay)))
        }
        Some(facts::DestinationFact::Occupied { obstacle: facts::Obstacle::Repository }) => {
            let blocker = staying(path, |_| Blocker::Obstacle(facts::Obstacle::Repository));
            (BTreeSet::from([path.clone()]), blocker)
        }
        Some(facts::DestinationFact::Occupied { obstacle }) => {
            (BTreeSet::new(), Some(Blocker::Obstacle(obstacle.clone())))
        }
        Some(facts::DestinationFact::InsideRepo { repo }) => {
            (BTreeSet::from([repo.clone()]), staying(repo, Blocker::InsideRepo))
        }
    };
    blocker.map_or(Ok(needs), Err)
}


// ===============
// === Blocker ===
// ===============

/// Why a repository can't land at its place in the layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Blocker {
    /// A repository this machine doesn't track is at the place.
    OtherRepo { origin: git::Origin },
    Obstacle(facts::Obstacle),
    /// The place is inside this repository.
    InsideRepo(domain::RepoPath),
    /// The directory at the place holds this repository.
    RepoInTheWay(domain::RepoPath),
    /// This repository was to make way, but stays: its removal is blocked or its own move can't happen, which the
    /// report says first.
    Staying(domain::RepoPath),
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let aside = "move it aside, then run `dev_sync pull` again";
        match self {
            Self::OtherRepo { origin: git::Origin::Url(url) } => write!(f, "a clone of {url} is there — {aside}"),
            Self::OtherRepo { origin: git::Origin::Missing } => {
                write!(f, "a repository with no origin remote is there — {aside}")
            }
            Self::Obstacle(obstacle) => write!(f, "{obstacle} — {aside}"),
            Self::InsideRepo(repo) => write!(
                f,
                "it would be inside the repository {repo} — move {repo} aside, then run `dev_sync pull` again"
            ),
            Self::RepoInTheWay(repo) => write!(f, "the repository {repo} is in the way — {aside}"),
            Self::Staying(repo) => write!(f, "the repository at {repo} is still there (see above)"),
        }
    }
}


// ====================
// === DiskConflict ===
// ====================

/// Something on disk that keeps a repository from landing at its place in the layout. The move or clone is skipped
/// and retried by the next pull.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DiskConflict {
    pub(crate) path: domain::RepoPath,
    /// Where the repository is, when it was to move there.
    pub(crate) from: Option<domain::RepoPath>,
    pub(crate) blocker: Blocker,
}

impl fmt::Display for DiskConflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (path, blocker) = (&self.path, &self.blocker);
        match &self.from {
            Some(from) => write!(f, "can't move {from} → {path}: {blocker}"),
            None => write!(f, "can't put {path} in place: {blocker}"),
        }
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::domain;
    use crate::fixtures;
    use crate::git;
    use crate::reconcile::facts;
    use crate::scan;
    use crate::state;
    use super::Action;
    use super::Blocker;
    use super::DiskConflict;
    use super::Plan;
    use super::plan;

    struct Case<'a> {
        base: &'a str,
        target: &'a str,
        observed: &'a str,
        facts: &'a [(&'a str, facts::DestinationFact)],
    }

    /// `path=url` items; the inode is the item's position. A base item ending in `!` is a blocked removal.
    fn run(case: &Case<'_>) -> anyhow::Result<Plan> {
        let base = case
            .base
            .split_whitespace()
            .zip(1..)
            .map(|(item, inode)| {
                let (item, status) = match item.strip_suffix('!') {
                    Some(item) => (item, state::KnownStatus::RemovalBlocked),
                    None => (item, state::KnownStatus::Synced),
                };
                let (path, url) = item.split_once('=').ok_or_else(|| anyhow::anyhow!("bad item {item}"))?;
                let id = domain::FileId { device: 1, inode };
                let known = state::KnownRepo { url: fixtures::url(url)?, id, status };
                Ok((fixtures::path(path)?, known))
            })
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
        let observed = case
            .observed
            .split_whitespace()
            .zip(1..)
            .map(|(item, inode)| {
                let (path, url) = item.split_once('=').ok_or_else(|| anyhow::anyhow!("bad item {item}"))?;
                let origin = match url {
                    "-" => git::Origin::Missing,
                    url => git::Origin::Url(fixtures::url(url)?),
                };
                Ok(scan::ObservedRepo { path: fixtures::path(path)?, id: domain::FileId { device: 1, inode }, origin })
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let destinations = case
            .facts
            .iter()
            .map(|(path, fact)| Ok((fixtures::path(path)?, fact.clone())))
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
        Ok(plan(
            &state::MachineState { repos: base },
            &fixtures::layout(case.target)?,
            &observed,
            &facts::DiskFacts { destinations },
        ))
    }

    fn actions(case: &Case<'_>) -> anyhow::Result<Vec<Action>> {
        let plan = run(case)?;
        anyhow::ensure!(plan.conflicts.is_empty(), "unexpected conflicts {:?}", plan.conflicts);
        Ok(plan.actions)
    }

    fn clone(path: &str, url: &str) -> anyhow::Result<Action> {
        Ok(Action::Clone { path: fixtures::path(path)?, url: fixtures::url(url)? })
    }

    fn moved(from: &str, to: &str) -> anyhow::Result<Action> {
        Ok(Action::Move { from: fixtures::path(from)?, to: fixtures::path(to)? })
    }

    fn conflict(path: &str, from: Option<&str>, blocker: Blocker) -> anyhow::Result<DiskConflict> {
        Ok(DiskConflict { path: fixtures::path(path)?, from: from.map(fixtures::path).transpose()?, blocker })
    }

    fn directory(repos: &[&str]) -> anyhow::Result<facts::DestinationFact> {
        let repos = repos.iter().copied().map(fixtures::path).collect::<anyhow::Result<_>>()?;
        Ok(facts::DestinationFact::Directory { repos })
    }

    const OCCUPIED_BY_REPO: facts::DestinationFact =
        facts::DestinationFact::Occupied { obstacle: facts::Obstacle::Repository };

    #[test]
    fn plan_1_clones_a_pending_repo() -> anyhow::Result<()> {
        let case = Case { base: "", target: "a=u1", observed: "", facts: &[("a", facts::DestinationFact::Free)] };
        assert_eq!(actions(&case)?, vec![clone("a", "u1")?]);
        Ok(())
    }

    #[test]
    fn plan_2_adopts_a_matching_clone() -> anyhow::Result<()> {
        let case = Case { base: "", target: "a=u1", observed: "a=u1", facts: &[("a", OCCUPIED_BY_REPO)] };
        assert_eq!(actions(&case)?, vec![Action::Adopt { path: fixtures::path("a")? }]);
        Ok(())
    }

    #[test]
    fn plan_3_another_clone_in_the_way_is_a_conflict() -> anyhow::Result<()> {
        let case = Case { base: "", target: "a=u1", observed: "a=u2", facts: &[("a", OCCUPIED_BY_REPO)] };
        let plan = run(&case)?;
        assert_eq!(plan.actions, vec![]);
        let origin = git::Origin::Url(fixtures::url("u2")?);
        assert_eq!(plan.conflicts, vec![conflict("a", None, Blocker::OtherRepo { origin })?]);
        Ok(())
    }

    #[test]
    fn plan_4_removes() -> anyhow::Result<()> {
        let case = Case { base: "a=u1", target: "", observed: "a=u1", facts: &[] };
        assert_eq!(actions(&case)?, vec![Action::Remove { path: fixtures::path("a")? }]);
        Ok(())
    }

    #[test]
    fn plan_5_moves() -> anyhow::Result<()> {
        let case = Case {
            base: "a=u1",
            target: "b=u1",
            observed: "a=u1",
            facts: &[("b", facts::DestinationFact::Free)],
        };
        assert_eq!(actions(&case)?, vec![moved("a", "b")?]);
        Ok(())
    }

    #[test]
    fn plan_6_swaps_with_two_moves() -> anyhow::Result<()> {
        let case = Case {
            base: "a=u1 b=u2",
            target: "a=u2 b=u1",
            observed: "a=u1 b=u2",
            facts: &[("a", OCCUPIED_BY_REPO), ("b", OCCUPIED_BY_REPO)],
        };
        assert_eq!(actions(&case)?, vec![moved("a", "b")?, moved("b", "a")?]);
        Ok(())
    }

    #[test]
    fn plan_7_follows_a_chain() -> anyhow::Result<()> {
        let case = Case {
            base: "a=u1 b=u2",
            target: "b=u1 c=u2",
            observed: "a=u1 b=u2",
            facts: &[("b", OCCUPIED_BY_REPO), ("c", facts::DestinationFact::Free)],
        };
        assert_eq!(actions(&case)?, vec![moved("a", "b")?, moved("b", "c")?]);
        Ok(())
    }

    #[test]
    fn plan_8_sets_a_new_url() -> anyhow::Result<()> {
        let case = Case { base: "a=u1", target: "a=u2", observed: "a=u1", facts: &[("a", OCCUPIED_BY_REPO)] };
        assert_eq!(actions(&case)?, vec![Action::SetUrl { path: fixtures::path("a")?, url: fixtures::url("u2")? }]);
        Ok(())
    }

    #[test]
    fn plan_9_a_non_empty_directory_in_the_way_is_a_conflict() -> anyhow::Result<()> {
        let fact = facts::DestinationFact::Occupied { obstacle: facts::Obstacle::NonEmptyDirectory };
        let plan = run(&Case { base: "", target: "a=u1", observed: "", facts: &[("a", fact)] })?;
        assert_eq!(plan.actions, vec![]);
        let blocker = Blocker::Obstacle(facts::Obstacle::NonEmptyDirectory);
        assert_eq!(plan.conflicts, vec![conflict("a", None, blocker)?]);
        Ok(())
    }

    #[test]
    fn plan_10_a_clone_inside_a_repo_is_a_conflict() -> anyhow::Result<()> {
        let fact = facts::DestinationFact::InsideRepo { repo: fixtures::path("x")? };
        let plan = run(&Case { base: "", target: "x/a=u1", observed: "x=u9", facts: &[("x/a", fact)] })?;
        assert_eq!(plan.actions, vec![]);
        assert_eq!(plan.conflicts, vec![conflict("x/a", None, Blocker::InsideRepo(fixtures::path("x")?))?]);
        Ok(())
    }

    #[test]
    fn plan_11_retries_a_blocked_removal() -> anyhow::Result<()> {
        let case = Case { base: "a=u1! b=u2", target: "b=u2", observed: "a=u1 b=u2", facts: &[] };
        assert_eq!(actions(&case)?, vec![Action::Remove { path: fixtures::path("a")? }]);
        Ok(())
    }

    #[test]
    fn plan_clones_into_an_empty_directory() -> anyhow::Result<()> {
        let case = Case { base: "", target: "a=u1", observed: "", facts: &[("a", directory(&[])?)] };
        assert_eq!(actions(&case)?, vec![clone("a", "u1")?]);
        Ok(())
    }

    #[test]
    fn plan_a_move_onto_a_repo_that_can_not_leave_is_dropped_too() -> anyhow::Result<()> {
        let file = facts::DestinationFact::Occupied { obstacle: facts::Obstacle::File };
        let case = Case {
            base: "a=u1 b=u2",
            target: "b=u1 c=u2",
            observed: "a=u1 b=u2",
            facts: &[("b", OCCUPIED_BY_REPO), ("c", file)],
        };
        let plan = run(&case)?;
        assert_eq!(plan.actions, vec![]);
        let expected = vec![
            conflict("c", Some("b"), Blocker::Obstacle(facts::Obstacle::File))?,
            conflict("b", Some("a"), Blocker::Staying(fixtures::path("b")?))?,
        ];
        assert_eq!(plan.conflicts, expected);
        Ok(())
    }

    #[test]
    fn plan_a_directory_holding_only_leaving_repos_is_no_obstacle() -> anyhow::Result<()> {
        let case = Case {
            base: "x/c=u2 y=u3",
            target: "x=u3 z=u2",
            observed: "x/c=u2 y=u3",
            facts: &[("x", directory(&["x/c"])?), ("z", facts::DestinationFact::Free)],
        };
        assert_eq!(actions(&case)?, vec![moved("x/c", "z")?, moved("y", "x")?]);
        Ok(())
    }

    #[test]
    fn plan_a_directory_holding_a_repo_that_stays_is_a_conflict() -> anyhow::Result<()> {
        let plan = run(&Case { base: "", target: "x=u1", observed: "x/c=-", facts: &[("x", directory(&["x/c"])?)] })?;
        assert_eq!(plan.actions, vec![]);
        assert_eq!(plan.conflicts, vec![conflict("x", None, Blocker::RepoInTheWay(fixtures::path("x/c")?))?]);
        Ok(())
    }

    #[test]
    fn conflicts_say_what_is_in_the_way_and_what_to_do() -> anyhow::Result<()> {
        let obstacle = conflict("b", Some("a"), Blocker::Obstacle(facts::Obstacle::File))?;
        assert_eq!(
            obstacle.to_string(),
            "can't move a → b: a file is in the way — move it aside, then run `dev_sync pull` again"
        );
        let staying = conflict("x/c", None, Blocker::Staying(fixtures::path("x")?))?;
        assert_eq!(staying.to_string(), "can't put x/c in place: the repository at x is still there (see above)");
        let nested = conflict("x/c", None, Blocker::InsideRepo(fixtures::path("x")?))?;
        assert_eq!(
            nested.to_string(),
            "can't put x/c in place: it would be inside the repository x — move x aside, then run `dev_sync pull` again"
        );
        Ok(())
    }

    #[test]
    fn plan_drops_a_move_whose_destination_is_taken() -> anyhow::Result<()> {
        let fact = facts::DestinationFact::Occupied { obstacle: facts::Obstacle::File };
        let plan = run(&Case { base: "a=u1", target: "b=u1", observed: "a=u1", facts: &[("b", fact)] })?;
        assert_eq!(plan.actions, vec![]);
        assert_eq!(plan.conflicts.len(), 1);
        Ok(())
    }

    #[test]
    fn plan_allows_a_destination_inside_a_leaving_repo() -> anyhow::Result<()> {
        let fact = facts::DestinationFact::InsideRepo { repo: fixtures::path("a")? };
        let case = Case { base: "a=u1", target: "a/b=u2", observed: "a=u1", facts: &[("a/b", fact)] };
        assert_eq!(actions(&case)?, vec![Action::Remove { path: fixtures::path("a")? }, clone("a/b", "u2")?]);
        Ok(())
    }

    #[test]
    fn plan_adopting_a_path_cancels_its_departure() -> anyhow::Result<()> {
        let case = Case { base: "a=u1", target: "a=u2", observed: "a=u2", facts: &[("a", OCCUPIED_BY_REPO)] };
        assert_eq!(actions(&case)?, vec![Action::Adopt { path: fixtures::path("a")? }]);
        Ok(())
    }

    #[test]
    fn plan_a_local_only_repo_in_the_way_is_a_conflict() -> anyhow::Result<()> {
        let plan = run(&Case { base: "", target: "a=u1", observed: "a=-", facts: &[("a", OCCUPIED_BY_REPO)] })?;
        let blocker = Blocker::OtherRepo { origin: git::Origin::Missing };
        assert_eq!(plan.conflicts, vec![conflict("a", None, blocker)?]);
        Ok(())
    }
}
