use std::collections::BTreeSet;
use std::fmt;

use crate::domain;
use crate::layout::change;
use crate::layout::model;


// ====================
// === MergeOutcome ===
// ====================

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum MergeOutcome {
    Clean(model::Layout),
    Conflicted(ConflictedMerge),
}


// =======================
// === ConflictedMerge ===
// =======================

/// A merge with conflicts: everything that merged cleanly, plus each conflict to resolve by hand.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConflictedMerge {
    /// The merge result without any path a conflict touches.
    pub(crate) resolved: model::Layout,
    /// Ordered by first touched path.
    pub(crate) conflicts: Vec<Conflict>,
}


// ================
// === Conflict ===
// ================

/// Local and incoming changes that touch overlapping paths.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Conflict {
    pub(crate) local: Vec<change::Change>,
    pub(crate) incoming: Vec<change::Change>,
    /// What the local layout has at the touched paths.
    pub(crate) local_repos: Vec<model::LayoutRepo>,
    /// What the incoming layout has at the touched paths.
    pub(crate) incoming_repos: Vec<model::LayoutRepo>,
}

impl Conflict {
    fn touched_paths(&self) -> BTreeSet<&domain::RepoPath> {
        touched_paths(&self.local, &self.incoming)
    }
}

fn touched_paths<'a>(local: &'a [change::Change], incoming: &'a [change::Change]) -> BTreeSet<&'a domain::RepoPath> {
    local.iter().chain(incoming).flat_map(change::Change::paths).collect()
}

/// A one-line description, specific for the common shapes.
impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.local.as_slice(), self.incoming.as_slice()) {
            ([change::Change::Move { from, to: mine, .. }], [change::Change::Move { from: source, to: theirs, .. }])
                if from == source =>
            {
                write!(
                    f,
                    "{:?}: moved to {:?} locally, to {:?} in the incoming change",
                    from.as_str(),
                    mine.as_str(),
                    theirs.as_str(),
                )
            }
            ([change::Change::Add { path, url: mine }], [change::Change::Add { path: other, url: theirs }])
                if path == other =>
            {
                write!(f, "{:?}: added locally as {mine}, incoming as {theirs}", path.as_str())
            }
            ([change::Change::SetUrl { path, to: mine, .. }], [change::Change::SetUrl { path: other, to: theirs, .. }])
                if path == other =>
            {
                write!(f, "{:?}: origin changed locally to {mine}, incoming to {theirs}", path.as_str())
            }
            ([change::Change::Move { from, to, .. }], [change::Change::Remove { path, .. }]) if from == path => {
                write!(f, "{:?}: moved to {:?} locally, removed in the incoming change", from.as_str(), to.as_str())
            }
            ([change::Change::Remove { path, .. }], [change::Change::Move { from, to, .. }]) if from == path => {
                write!(f, "{:?}: removed locally, moved to {:?} in the incoming change", path.as_str(), to.as_str())
            }
            ([change::Change::Remove { path, .. }], [change::Change::SetUrl { path: other, to, .. }])
                if path == other =>
            {
                write!(f, "{:?}: removed locally, origin changed to {to} in the incoming change", path.as_str())
            }
            ([change::Change::SetUrl { path, to, .. }], [change::Change::Remove { path: other, .. }])
                if path == other =>
            {
                write!(f, "{:?}: origin changed to {to} locally, removed in the incoming change", path.as_str())
            }
            (local, incoming) => write!(f, "local: {}; incoming: {}", describe(local), describe(incoming)),
        }
    }
}

fn describe(changes: &[change::Change]) -> String {
    match changes {
        [] => "nothing".to_owned(),
        _ => changes.iter().map(change::Change::to_string).collect::<Vec<_>>().join(", "),
    }
}


// =============
// === merge ===
// =============

/// Three-way merge of layouts. Changes both sides made identically are kept once. A local change and an incoming
/// change whose paths overlap conflict; conflicts that share changes form one group. Everything else applies cleanly.
pub(crate) fn merge(base: &model::Layout, local: &model::Layout, incoming: &model::Layout) -> MergeOutcome {
    let local_changes = change::diff(base, local);
    let incoming_changes = change::diff(base, incoming);
    let common = local_changes.iter().filter(|change| incoming_changes.contains(change)).cloned().collect::<Vec<_>>();
    let local_only = local_changes.into_iter().filter(|change| !common.contains(change)).collect::<Vec<_>>();
    let incoming_only = incoming_changes.into_iter().filter(|change| !common.contains(change)).collect::<Vec<_>>();
    let groups = conflict_groups(&local_only, &incoming_only);
    let grouped = groups.iter().flat_map(|group| group.local.iter().chain(&group.incoming)).collect::<Vec<_>>();
    let sided = |side: Side| move |change: change::Change| Sided { side, change };
    let clean = common
        .into_iter()
        .map(sided(Side::Both))
        .chain(local_only.iter().filter(|change| !grouped.contains(change)).cloned().map(sided(Side::Local)))
        .chain(incoming_only.iter().filter(|change| !grouped.contains(change)).cloned().map(sided(Side::Incoming)))
        .collect::<Vec<_>>();
    let applied = apply_clean(base, clean);
    let leftover = (!applied.rejected.is_empty()).then(|| Group::from_sided(&applied.rejected));
    let groups = groups.into_iter().chain(leftover).collect::<Vec<_>>();
    match groups.is_empty() {
        true => MergeOutcome::Clean(applied.layout),
        false => MergeOutcome::Conflicted(conflicted(&applied.layout, local, incoming, groups)),
    }
}

fn conflicted(
    result: &model::Layout,
    local: &model::Layout,
    incoming: &model::Layout,
    groups: Vec<Group>,
) -> ConflictedMerge {
    let at_paths = |layout: &model::Layout, paths: &BTreeSet<&domain::RepoPath>| {
        layout.repos().filter(|repo| paths.contains(&repo.path)).collect::<Vec<_>>()
    };
    let mut conflicts = groups
        .into_iter()
        .map(|group| {
            let touched = touched_paths(&group.local, &group.incoming);
            let local_repos = at_paths(local, &touched);
            let incoming_repos = at_paths(incoming, &touched);
            Conflict { local: group.local, incoming: group.incoming, local_repos, incoming_repos }
        })
        .collect::<Vec<_>>();
    conflicts.sort_by(|left, right| left.touched_paths().first().cmp(&right.touched_paths().first()));
    let touched = conflicts.iter().flat_map(Conflict::touched_paths).cloned().collect::<BTreeSet<_>>();
    let resolved = result.without(&touched);
    ConflictedMerge { resolved, conflicts }
}


// =====================
// === stray_changes ===
// =====================

/// The changes a resolution of `outcome` makes outside its conflicts. Each conflict may be settled either way, but
/// everything that merged cleanly has to stay as merged; an empty result means the resolution is sound.
pub(crate) fn stray_changes(outcome: &MergeOutcome, resolution: &model::Layout) -> Vec<change::Change> {
    let touched = match outcome {
        MergeOutcome::Clean(_) => BTreeSet::new(),
        MergeOutcome::Conflicted(conflicted) => {
            conflicted.conflicts.iter().flat_map(Conflict::touched_paths).cloned().collect()
        }
    };
    let clean = match outcome {
        MergeOutcome::Clean(merged) => merged,
        MergeOutcome::Conflicted(conflicted) => &conflicted.resolved,
    };
    change::diff(&clean.without(&touched), &resolution.without(&touched))
}


// =============
// === Group ===
// =============

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Node {
    Local(usize),
    Incoming(usize),
}

struct Group {
    local: Vec<change::Change>,
    incoming: Vec<change::Change>,
}

impl Group {
    fn from_sided(changes: &[Sided]) -> Self {
        let on = |sides: [Side; 2]| {
            changes.iter().filter(|c| sides.contains(&c.side)).map(|c| c.change.clone()).collect::<Vec<_>>()
        };
        Self { local: on([Side::Local, Side::Both]), incoming: on([Side::Incoming, Side::Both]) }
    }
}

/// Connected components of the "paths overlap" relation between local and incoming changes, ignoring changes that
/// overlap nothing.
fn conflict_groups(local: &[change::Change], incoming: &[change::Change]) -> Vec<Group> {
    let overlap = |a: &change::Change, b: &change::Change| {
        a.paths().iter().any(|p| b.paths().iter().any(|q| p.overlaps(q)))
    };
    let neighbors = |node: Node| -> Vec<Node> {
        match node {
            Node::Local(index) => local.get(index).map_or_else(Vec::new, |change| {
                (0..incoming.len())
                    .filter(|other| incoming.get(*other).is_some_and(|theirs| overlap(change, theirs)))
                    .map(Node::Incoming)
                    .collect()
            }),
            Node::Incoming(index) => incoming.get(index).map_or_else(Vec::new, |change| {
                (0..local.len())
                    .filter(|other| local.get(*other).is_some_and(|mine| overlap(mine, change)))
                    .map(Node::Local)
                    .collect()
            }),
        }
    };
    let mut seen = BTreeSet::new();
    let mut groups = Vec::new();
    for start in (0..local.len()).map(Node::Local) {
        if seen.contains(&start) || neighbors(start).is_empty() {
            continue;
        }
        let mut members = BTreeSet::new();
        let mut queue = vec![start];
        while let Some(node) = queue.pop() {
            if seen.insert(node) {
                members.insert(node);
                queue.extend(neighbors(node));
            }
        }
        let pick = |node: &Node| match node {
            Node::Local(index) => local.get(*index).cloned().map(|change| Sided { side: Side::Local, change }),
            Node::Incoming(index) => incoming.get(*index).cloned().map(|change| Sided { side: Side::Incoming, change }),
        };
        groups.push(Group::from_sided(&members.iter().filter_map(pick).collect::<Vec<_>>()));
    }
    groups
}


// =============
// === Sided ===
// =============

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Side {
    Local,
    Incoming,
    Both,
}

#[derive(Clone, Debug)]
struct Sided {
    side: Side,
    change: change::Change,
}


// ==================
// === CleanApply ===
// ==================

struct CleanApply {
    layout: model::Layout,
    rejected: Vec<Sided>,
}

/// Applies the clean changes to `base`. Changes that the batch rejects (which shouldn't happen) are set aside and the
/// rest is applied again, so the merge never panics and never applies half a batch.
fn apply_clean(base: &model::Layout, clean: Vec<Sided>) -> CleanApply {
    let mut pending = clean;
    let mut rejected = Vec::new();
    loop {
        let changes = pending.iter().map(|sided| sided.change.clone()).collect::<Vec<_>>();
        match base.apply(&changes) {
            model::Applied::Ok(layout) => break CleanApply { layout, rejected },
            model::Applied::Rejected(rejections) => {
                let bad = rejections.into_iter().map(|rejection| rejection.change).collect::<Vec<_>>();
                let (dropped, kept): (Vec<_>, Vec<_>) = pending.into_iter().partition(|s| bad.contains(&s.change));
                let stuck = dropped.is_empty();
                rejected.extend(dropped);
                pending = kept;
                if stuck {
                    rejected.append(&mut pending);
                    break CleanApply { layout: base.clone(), rejected };
                }
            }
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
    use crate::layout::model::LayoutRepo;
    use super::ConflictedMerge;
    use super::MergeOutcome;
    use super::merge;
    use super::stray_changes;

    fn run(base: &str, local: &str, incoming: &str) -> anyhow::Result<MergeOutcome> {
        Ok(merge(&fixtures::layout(base)?, &fixtures::layout(local)?, &fixtures::layout(incoming)?))
    }

    fn clean(base: &str, local: &str, incoming: &str, expected: &str) -> anyhow::Result<()> {
        let outcome = run(base, local, incoming)?;
        assert_eq!(outcome, MergeOutcome::Clean(fixtures::layout(expected)?), "{base} / {local} / {incoming}");
        Ok(())
    }

    fn conflicted(base: &str, local: &str, incoming: &str) -> anyhow::Result<ConflictedMerge> {
        match run(base, local, incoming)? {
            MergeOutcome::Conflicted(conflicted) => Ok(conflicted),
            MergeOutcome::Clean(layout) => {
                anyhow::bail!("{base} / {local} / {incoming}: expected a conflict, got {layout:?}")
            }
        }
    }

    fn repos(spec: &str) -> anyhow::Result<Vec<LayoutRepo>> {
        Ok(fixtures::layout(spec)?.repos().collect())
    }

    #[test]
    fn row_1_independent_additions_merge() -> anyhow::Result<()> {
        clean("", "a=u1", "b=u2", "a=u1 b=u2")
    }

    #[test]
    fn row_2_identical_additions_merge() -> anyhow::Result<()> {
        clean("", "a=u1", "a=u1", "a=u1")
    }

    #[test]
    fn row_3_additions_with_different_urls_conflict() -> anyhow::Result<()> {
        let merged = conflicted("", "a=u1", "a=u2")?;
        assert_eq!(merged.resolved, fixtures::layout("")?);
        assert_eq!(merged.conflicts.len(), 1);
        let conflict = merged.conflicts.first().ok_or_else(|| anyhow::anyhow!("no conflict"))?;
        assert_eq!(conflict.local_repos, repos("a=u1")?);
        assert_eq!(conflict.incoming_repos, repos("a=u2")?);
        Ok(())
    }

    #[test]
    fn row_4_identical_removals_merge() -> anyhow::Result<()> {
        clean("a=u1", "", "", "")
    }

    #[test]
    fn row_5_a_one_sided_removal_merges() -> anyhow::Result<()> {
        clean("a=u1 b=u2", "b=u2", "a=u1 b=u2", "b=u2")
    }

    #[test]
    fn row_6_removal_against_url_change_conflicts_with_an_empty_local_side() -> anyhow::Result<()> {
        let merged = conflicted("a=u1", "", "a=u2")?;
        assert_eq!(merged.resolved, fixtures::layout("")?);
        let conflict = merged.conflicts.first().ok_or_else(|| anyhow::anyhow!("no conflict"))?;
        assert_eq!(conflict.local_repos, vec![]);
        assert_eq!(conflict.incoming_repos, repos("a=u2")?);
        Ok(())
    }

    #[test]
    fn row_7_a_one_sided_move_merges() -> anyhow::Result<()> {
        clean("a=u1", "x/a=u1", "a=u1", "x/a=u1")
    }

    #[test]
    fn row_8_moves_to_different_places_conflict_in_one_group() -> anyhow::Result<()> {
        let merged = conflicted("a=u1", "x=u1", "y=u1")?;
        assert_eq!(merged.resolved, fixtures::layout("")?);
        assert_eq!(merged.conflicts.len(), 1);
        let conflict = merged.conflicts.first().ok_or_else(|| anyhow::anyhow!("no conflict"))?;
        assert_eq!(conflict.local_repos, repos("x=u1")?);
        assert_eq!(conflict.incoming_repos, repos("y=u1")?);
        assert_eq!(conflict.to_string(), "\"a\": moved to \"x\" locally, to \"y\" in the incoming change");
        Ok(())
    }

    #[test]
    fn row_9_identical_moves_merge() -> anyhow::Result<()> {
        clean("a=u1", "x=u1", "x=u1", "x=u1")
    }

    #[test]
    fn row_10_move_against_removal_conflicts() -> anyhow::Result<()> {
        let merged = conflicted("a=u1", "x=u1", "")?;
        let conflict = merged.conflicts.first().ok_or_else(|| anyhow::anyhow!("no conflict"))?;
        assert_eq!(conflict.local_repos, repos("x=u1")?);
        assert_eq!(conflict.incoming_repos, vec![]);
        Ok(())
    }

    #[test]
    fn row_11_overlapping_additions_conflict() -> anyhow::Result<()> {
        conflicted("", "a=u1", "a/b=u2").map(|_| ())
    }

    #[test]
    fn row_12_identical_url_changes_merge() -> anyhow::Result<()> {
        clean("a=u1", "a=u2", "a=u2", "a=u2")
    }

    #[test]
    fn row_13_different_url_changes_conflict() -> anyhow::Result<()> {
        let merged = conflicted("a=u1", "a=u2", "a=u3")?;
        let conflict = merged.conflicts.first().ok_or_else(|| anyhow::anyhow!("no conflict"))?;
        assert_eq!(conflict.to_string(), "\"a\": origin changed locally to u2, incoming to u3");
        Ok(())
    }

    #[test]
    fn row_14_chained_overlaps_form_one_group() -> anyhow::Result<()> {
        let merged = conflicted("a=u1 b=u2", "b=u2 c=u1", "a=u1 c=u2")?;
        assert_eq!(merged.conflicts.len(), 1);
        assert_eq!(merged.resolved, fixtures::layout("")?);
        Ok(())
    }

    #[test]
    fn row_15_a_conflict_keeps_the_clean_changes_around_it() -> anyhow::Result<()> {
        let merged = conflicted("a=u1", "a=u1 b=u2 c=u3", "a=u1 c=u4 d=u5")?;
        assert_eq!(merged.resolved, fixtures::layout("a=u1 b=u2 d=u5")?);
        assert_eq!(merged.conflicts.len(), 1);
        let conflict = merged.conflicts.first().ok_or_else(|| anyhow::anyhow!("no conflict"))?;
        assert_eq!(conflict.local, vec![fixtures::add("c", "u3")?]);
        assert_eq!(conflict.incoming, vec![fixtures::add("c", "u4")?]);
        assert_eq!(conflict.to_string(), "\"c\": added locally as u3, incoming as u4");
        Ok(())
    }

    #[test]
    fn row_16_a_move_between_clones_of_one_url_merges() -> anyhow::Result<()> {
        clean("a=u1 b=u1", "a=u1 x=u1", "a=u1 b=u1", "a=u1 x=u1")
    }

    #[test]
    fn separate_conflicts_are_ordered_by_path() -> anyhow::Result<()> {
        let merged = conflicted("", "b=u1 a=u1", "b=u2 a=u2")?;
        let firsts = merged
            .conflicts
            .iter()
            .filter_map(|c| c.local.first().map(Change::first_path))
            .collect::<Vec<_>>();
        assert_eq!(firsts, vec![&fixtures::path("a")?, &fixtures::path("b")?]);
        Ok(())
    }

    #[test]
    fn unusual_pairs_get_the_generic_description() -> anyhow::Result<()> {
        let merged = conflicted("a=u1", "x=u1", "a=u2")?;
        let conflict = merged.conflicts.first().ok_or_else(|| anyhow::anyhow!("no conflict"))?;
        assert_eq!(conflict.to_string(), "local: a → x; incoming: a: u1 → u2");
        Ok(())
    }

    #[test]
    fn row_17_overlaps_chain_through_one_incoming_change() -> anyhow::Result<()> {
        let merged = conflicted("x=u1", "a=u1 y=u2", "y=u1")?;
        assert_eq!(merged.conflicts.len(), 1);
        let conflict = merged.conflicts.first().ok_or_else(|| anyhow::anyhow!("no conflict"))?;
        assert_eq!(conflict.local, vec![fixtures::move_to("x", "a", "u1")?, fixtures::add("y", "u2")?]);
        assert_eq!(conflict.incoming, vec![fixtures::move_to("x", "y", "u1")?]);
        Ok(())
    }

    #[test]
    fn describes_removal_against_move_and_url_change() -> anyhow::Result<()> {
        let describe = |base, local, incoming| -> anyhow::Result<String> {
            let merged = conflicted(base, local, incoming)?;
            Ok(merged.conflicts.first().map(ToString::to_string).unwrap_or_default())
        };
        assert_eq!(describe("a=u1", "", "x=u1")?, "\"a\": removed locally, moved to \"x\" in the incoming change");
        assert_eq!(describe("a=u1", "x=u1", "")?, "\"a\": moved to \"x\" locally, removed in the incoming change");
        assert_eq!(
            describe("a=u1", "", "a=u2")?,
            "\"a\": removed locally, origin changed to u2 in the incoming change"
        );
        assert_eq!(
            describe("a=u1", "a=u2", "")?,
            "\"a\": origin changed to u2 locally, removed in the incoming change"
        );
        Ok(())
    }

    #[test]
    fn a_resolution_may_only_change_the_conflicts() -> anyhow::Result<()> {
        let outcome = run("a=u1", "a=u1 b=u2 c=u3", "a=u1 c=u4 d=u5")?;
        assert_eq!(stray_changes(&outcome, &fixtures::layout("a=u1 b=u2 d=u5 c=u3")?), vec![]);
        assert_eq!(stray_changes(&outcome, &fixtures::layout("a=u1 b=u2 d=u5")?), vec![]);
        assert_eq!(stray_changes(&outcome, &fixtures::layout("a=u1 b=u2 c=u4")?), vec![fixtures::remove("d", "u5")?]);
        let extra = fixtures::layout("a=u1 b=u2 d=u5 c=u3 e=u9")?;
        assert_eq!(stray_changes(&outcome, &extra), vec![fixtures::add("e", "u9")?]);
        let clean = run("", "a=u1", "b=u2")?;
        assert_eq!(stray_changes(&clean, &fixtures::layout("a=u1 b=u2")?), vec![]);
        assert_eq!(stray_changes(&clean, &fixtures::layout("a=u1")?), vec![fixtures::remove("b", "u2")?]);
        Ok(())
    }
}
