//! Local layout changes: detected from the disk, applied to the committed snapshot, and the base that follows
//! (§8.2, §8.3, §8.12).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::domain;
use crate::git;
use crate::layout;
use crate::scan;
use crate::shell;
use crate::state;


// ====================
// === LocalChanges ===
// ====================

#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[must_use]
pub(crate) struct LocalChanges {
    /// Sorted by first path.
    pub(crate) changes: Vec<layout::Change>,
    /// Clones without an `origin` that the base doesn't know. They can't be synced.
    pub(crate) local_only: Vec<domain::RepoPath>,
}

/// Compares the base with a fresh scan. A clone is matched to its base entry by the id of its `.git` directory
/// first — for every clone, so that a new clone at an old path can't take over the entry of a clone that moved away
/// — and then by path. An unmatched base entry was removed.
pub(crate) fn detect(base: &state::MachineState, observed: &[scan::ObservedRepo]) -> anyhow::Result<LocalChanges> {
    let by_id = base.repos.iter().map(|(path, known)| (known.id, path)).collect::<BTreeMap<_, _>>();
    let mut matched = BTreeSet::new();
    let mut changes = Vec::new();
    let mut unmatched = Vec::new();
    for repo in observed {
        match by_id.get(&repo.id).and_then(|path| base.repos.get(*path).map(|known| Known { path, known })) {
            Some(entry) => {
                matched.insert(entry.path);
                changes.extend(changes_for(repo, &entry)?);
            }
            None => unmatched.push(repo),
        }
    }
    let mut local_only = Vec::new();
    for repo in unmatched {
        let at_path = base.repos.get_key_value(&repo.path).filter(|(path, _)| !matched.contains(path));
        match (at_path, &repo.origin) {
            (Some((path, known)), _) => {
                matched.insert(path);
                changes.extend(changes_for(repo, &Known { path, known })?);
            }
            (None, git::Origin::Url(url)) => {
                changes.push(layout::Change::Add { path: repo.path.clone(), url: url.clone() });
            }
            (None, git::Origin::Missing) => local_only.push(repo.path.clone()),
        }
    }
    let removed = base.repos.iter().filter(|(path, _)| !matched.contains(path));
    changes.extend(removed.map(|(path, known)| layout::Change::Remove { path: path.clone(), url: known.url.clone() }));
    changes.sort_by(|left, right| left.first_path().cmp(right.first_path()));
    Ok(LocalChanges { changes, local_only })
}

struct Known<'a> {
    path: &'a domain::RepoPath,
    known: &'a state::KnownRepo,
}

/// The changes between a base entry and the clone matched to it.
fn changes_for(repo: &scan::ObservedRepo, entry: &Known<'_>) -> anyhow::Result<Vec<layout::Change>> {
    let url = match &repo.origin {
        git::Origin::Url(url) => Ok(url),
        git::Origin::Missing => Err(anyhow::anyhow!(
            "{path} had origin {old}, but its origin remote is gone. Restore it (`git -C {repo} remote add origin \
             {url} && git -C {repo} fetch origin`, then `git -C {repo} branch --set-upstream-to=origin/<branch>` for \
             each branch that tracked it) or move the repository out of the workspace",
            repo = shell::word(repo.path.as_str()),
            url = shell::word(entry.known.url.as_str()),
            path = repo.path,
            old = entry.known.url,
        )),
    }?;
    let same_url = *url == entry.known.url;
    Ok(match (repo.path == *entry.path, same_url) {
        (true, true) => vec![],
        (true, false) => {
            vec![layout::Change::SetUrl { path: repo.path.clone(), from: entry.known.url.clone(), to: url.clone() }]
        }
        (false, true) => {
            vec![layout::Change::Move { from: entry.path.clone(), to: repo.path.clone(), url: url.clone() }]
        }
        (false, false) => vec![
            layout::Change::Remove { path: entry.path.clone(), url: entry.known.url.clone() },
            layout::Change::Add { path: repo.path.clone(), url: url.clone() },
        ],
    })
}


// =====================
// === LocalConflict ===
// =====================

/// A local change that can't be recorded because it clashes with the committed layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LocalConflict {
    pub(crate) path: domain::RepoPath,
    /// What clashes and how to fix it on disk.
    pub(crate) message: String,
}


// ================
// === Recorded ===
// ================

#[derive(Clone, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum Recorded {
    Unchanged,
    Changed { snapshot: layout::Layout, applied: Vec<layout::Change> },
    Conflicts(Vec<LocalConflict>),
}

/// Rewrites each local change against the snapshot, which may differ from what this machine last saw (pending
/// clones, changes merged but not yet on disk), then applies the result as one batch. A removal or move only takes
/// a path out of the snapshot when the snapshot has the same repository (URL) there: if it holds another one, that
/// repository isn't what left this machine.
pub(crate) fn apply_to_snapshot(snapshot: &layout::Layout, changes: &[layout::Change]) -> Recorded {
    let leaving = changes
        .iter()
        .filter_map(|change| match change {
            layout::Change::Remove { path, url } | layout::Change::Move { from: path, url, .. } => {
                snapshot.get(path).filter(|entry| entry.url == *url).map(|_| path)
            }
            layout::Change::Add { .. } | layout::Change::SetUrl { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    let rewritten = changes.iter().map(|change| rewrite(snapshot, &leaving, change)).collect::<Vec<_>>();
    let conflicts = rewritten
        .iter()
        .filter_map(|rewritten| match rewritten {
            Rewritten::Conflict(conflict) => Some(conflict.clone()),
            Rewritten::Changes(_) => None,
        })
        .collect::<Vec<_>>();
    let batch = rewritten
        .into_iter()
        .flat_map(|rewritten| match rewritten {
            Rewritten::Changes(changes) => changes,
            Rewritten::Conflict(_) => Vec::new(),
        })
        .collect::<Vec<_>>();
    match (conflicts.is_empty(), batch.is_empty()) {
        (false, _) => Recorded::Conflicts(conflicts),
        (true, true) => Recorded::Unchanged,
        (true, false) => match snapshot.apply(&batch) {
            layout::Applied::Ok(result) if result == *snapshot => Recorded::Unchanged,
            layout::Applied::Ok(result) => Recorded::Changed { snapshot: result, applied: batch },
            layout::Applied::Rejected(rejections) => Recorded::Conflicts(rejections.iter().map(explain).collect()),
        },
    }
}

/// What a local change becomes against the snapshot.
enum Rewritten {
    /// Nothing, when the snapshot already has it.
    Changes(Vec<layout::Change>),
    Conflict(LocalConflict),
}

fn rewrite(snapshot: &layout::Layout, leaving: &BTreeSet<&domain::RepoPath>, change: &layout::Change) -> Rewritten {
    let url_at = |path: &domain::RepoPath| snapshot.get(path).map(|entry| entry.url.clone());
    match change {
        layout::Change::Add { path, url } => match url_at(path) {
            Some(_) if leaving.contains(path) => Rewritten::Changes(vec![change.clone()]),
            Some(existing) if existing == *url => Rewritten::Changes(vec![]),
            Some(existing) => Rewritten::Conflict(LocalConflict {
                path: path.clone(),
                message: format!(
                    "{path} is in the layout as {existing}, but the clone on disk has origin {url} — point it back \
                     (`git -C {} remote set-url origin {}`) or move the clone out of the way",
                    shell::word(path.as_str()),
                    shell::word(existing.as_str()),
                ),
            }),
            None => Rewritten::Changes(vec![change.clone()]),
        },
        layout::Change::Remove { path, url } => Rewritten::Changes(match url_at(path) {
            Some(existing) if existing == *url => vec![change.clone()],
            Some(_) | None => vec![],
        }),
        layout::Change::Move { from, to, url } => {
            let source_is_this_clone = url_at(from).is_some_and(|source| source == *url);
            match url_at(to) {
                Some(existing) if !leaving.contains(to) && existing == *url => {
                    Rewritten::Changes(match source_is_this_clone {
                        true => vec![layout::Change::Remove { path: from.clone(), url: url.clone() }],
                        false => vec![],
                    })
                }
                Some(existing) if !leaving.contains(to) => Rewritten::Conflict(LocalConflict {
                    path: to.clone(),
                    message: format!(
                        "{from} was moved to {to}, but the layout has {existing} at {to} — move the clone somewhere \
                         else"
                    ),
                }),
                Some(_) | None => Rewritten::Changes(match source_is_this_clone {
                    true => vec![change.clone()],
                    false => vec![layout::Change::Add { path: to.clone(), url: url.clone() }],
                }),
            }
        }
        layout::Change::SetUrl { path, to, .. } => Rewritten::Changes(match url_at(path) {
            Some(from) if from != *to => vec![layout::Change::SetUrl { path: path.clone(), from, to: to.clone() }],
            Some(_) | None => vec![],
        }),
    }
}

fn explain(rejection: &layout::ChangeRejection) -> LocalConflict {
    let path = match &rejection.change {
        layout::Change::Move { to, .. } => to.clone(),
        layout::Change::Add { path, .. }
        | layout::Change::Remove { path, .. }
        | layout::Change::SetUrl { path, .. } => path.clone(),
    };
    let message = match &rejection.reason {
        layout::RejectionReason::Nested { with } => format!(
            "{path} and {with} would be nested — a repository inside another can't be synced; move {path} elsewhere \
             (or into a hidden folder to keep it out of the layout)"
        ),
        reason @ (layout::RejectionReason::PathAbsent
        | layout::RejectionReason::PathPresent
        | layout::RejectionReason::UrlMismatch) => {
            format!("can't record {}: {reason} — check `dev_sync status` and fix it on disk", rejection.change)
        }
    };
    LocalConflict { path, message }
}


// =================
// === next_base ===
// =================

/// The base after a record or a reconcile (§5.4): every clone on disk whose path and URL match the snapshot, every
/// blocked removal, and every failed removal as it was. Pending clones and local-only repos stay out, so a repo that
/// was never cloned is never mistaken for one the user deleted.
pub(crate) fn next_base(
    snapshot: &layout::Layout,
    base: &state::MachineState,
    observed: &[scan::ObservedRepo],
    newly_blocked: &BTreeSet<domain::RepoPath>,
) -> state::MachineState {
    let repos = observed
        .iter()
        .filter_map(|repo| {
            let url = match &repo.origin {
                git::Origin::Url(url) => Some(url),
                git::Origin::Missing => None,
            }?;
            let known = match snapshot.get(&repo.path) {
                Some(entry) if entry.url == *url => {
                    Some(state::KnownRepo { url: url.clone(), id: repo.id, status: state::KnownStatus::Synced })
                }
                Some(_) | None => base.repos.get(&repo.path).map(|known| state::KnownRepo {
                    url: known.url.clone(),
                    id: repo.id,
                    status: match newly_blocked.contains(&repo.path) {
                        true => state::KnownStatus::RemovalBlocked,
                        false => known.status,
                    },
                }),
            }?;
            Some((repo.path.clone(), known))
        })
        .collect();
    state::MachineState { repos }
}


// =====================
// === CommitMessage ===
// =====================

/// The first line of a layout commit stays within about this many characters.
const SUBJECT_WIDTH: usize = 72;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CommitMessage {
    pub(crate) subject: String,
    pub(crate) body: String,
}

/// `<host>: <changes>` (cut to about 72 characters with `(+N more)`), and a body listing every change with its URLs.
pub(crate) fn commit_message(host: &domain::HostName, changes: &[layout::Change]) -> CommitMessage {
    let items = changes.iter().map(short).collect::<Vec<_>>();
    let prefix = format!("{host}: ");
    let fitting = items.iter().enumerate().scan(prefix.chars().count(), |width, (index, item)| {
        let separator = if index == 0 { "" } else { ", " };
        *width += separator.len() + item.chars().count();
        Some(index == 0 || *width <= SUBJECT_WIDTH)
    });
    let shown = fitting.take_while(|fits| *fits).count();
    let listed = items.iter().take(shown).map(String::as_str).collect::<Vec<_>>().join(", ");
    let hidden = items.len().saturating_sub(shown);
    let more = if hidden > 0 { format!(" (+{hidden} more)") } else { String::new() };
    let subject = format!("{prefix}{listed}{more}");
    let body = changes.iter().map(long).collect::<Vec<_>>().join("\n");
    CommitMessage { subject, body }
}

fn short(change: &layout::Change) -> String {
    match change {
        layout::Change::SetUrl { path, to, .. } => format!("{path}: {to}"),
        layout::Change::Add { .. } | layout::Change::Remove { .. } | layout::Change::Move { .. } => change.to_string(),
    }
}

fn long(change: &layout::Change) -> String {
    match change {
        layout::Change::Add { path, url } => format!("add {path} ({url})"),
        layout::Change::Remove { path, url } => format!("remove {path} ({url})"),
        layout::Change::Move { from, to, url } => format!("move {from} → {to} ({url})"),
        layout::Change::SetUrl { path, from, to } => format!("set origin of {path}: {from} → {to}"),
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;

    use crate::domain;
    use crate::fixtures;
    use crate::git;
    use crate::layout;
    use crate::scan;
    use crate::state;
    use super::LocalChanges;
    use super::LocalConflict;
    use super::Recorded;
    use super::apply_to_snapshot;
    use super::commit_message;
    use super::detect;
    use super::next_base;

    fn id(inode: u64) -> domain::FileId {
        domain::FileId { device: 1, inode }
    }

    /// A base from `path=url@inode` items; a trailing `!` marks a blocked removal.
    fn base(spec: &str) -> anyhow::Result<state::MachineState> {
        let repos = spec
            .split_whitespace()
            .map(|item| {
                let (item, status) = match item.strip_suffix('!') {
                    Some(item) => (item, state::KnownStatus::RemovalBlocked),
                    None => (item, state::KnownStatus::Synced),
                };
                let (pair, inode) = item.split_once('@').ok_or_else(|| anyhow::anyhow!("no @ in {item}"))?;
                let (path, url) = pair.split_once('=').ok_or_else(|| anyhow::anyhow!("no = in {item}"))?;
                let known = state::KnownRepo { url: fixtures::url(url)?, id: id(inode.parse()?), status };
                Ok((fixtures::path(path)?, known))
            })
            .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
        Ok(state::MachineState { repos })
    }

    /// Observed repos from `path=url@inode` items; `path=-@inode` has no origin.
    fn observed(spec: &str) -> anyhow::Result<Vec<scan::ObservedRepo>> {
        spec.split_whitespace()
            .map(|item| {
                let (pair, inode) = item.split_once('@').ok_or_else(|| anyhow::anyhow!("no @ in {item}"))?;
                let (path, url) = pair.split_once('=').ok_or_else(|| anyhow::anyhow!("no = in {item}"))?;
                let origin = match url {
                    "-" => git::Origin::Missing,
                    url => git::Origin::Url(fixtures::url(url)?),
                };
                Ok(scan::ObservedRepo { path: fixtures::path(path)?, id: id(inode.parse()?), origin })
            })
            .collect()
    }

    fn changes(base_spec: &str, observed_spec: &str) -> anyhow::Result<Vec<layout::Change>> {
        Ok(detect(&base(base_spec)?, &observed(observed_spec)?)?.changes)
    }

    #[test]
    fn detect_1_unchanged() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1", "a=u1@1")?, vec![]);
        Ok(())
    }

    #[test]
    fn detect_2_new() -> anyhow::Result<()> {
        assert_eq!(changes("", "a=u1@1")?, vec![fixtures::add("a", "u1")?]);
        Ok(())
    }

    #[test]
    fn detect_3_gone() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1", "")?, vec![fixtures::remove("a", "u1")?]);
        Ok(())
    }

    #[test]
    fn detect_4_moved() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1", "b=u1@1")?, vec![fixtures::move_to("a", "b", "u1")?]);
        Ok(())
    }

    #[test]
    fn detect_5_moved_with_a_new_url_is_a_removal_and_an_addition() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1", "b=u2@1")?, vec![fixtures::remove("a", "u1")?, fixtures::add("b", "u2")?]);
        Ok(())
    }

    #[test]
    fn detect_6_recloned_in_place() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1", "a=u1@2")?, vec![]);
        Ok(())
    }

    #[test]
    fn detect_7_new_url() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1", "a=u2@1")?, vec![fixtures::set_url("a", "u1", "u2")?]);
        Ok(())
    }

    #[test]
    fn detect_8_no_origin_and_unknown_is_local_only() -> anyhow::Result<()> {
        let detected = detect(&base("")?, &observed("a=-@1")?)?;
        assert_eq!(detected, LocalChanges { changes: vec![], local_only: vec![fixtures::path("a")?] });
        Ok(())
    }

    #[test]
    fn detect_9_a_known_repo_that_lost_its_origin_is_an_error() -> anyhow::Result<()> {
        for observed_spec in ["a=-@1", "b=-@1", "a=-@2"] {
            let message = detect(&base("a=u1@1")?, &observed(observed_spec)?).err().map(|e| format!("{e:#}"));
            let message = message.unwrap_or_default();
            assert!(message.contains("origin remote is gone"), "{observed_spec}: {message}");
            assert!(message.contains("remote add origin u1"), "{observed_spec}: {message}");
            assert!(message.contains("branch --set-upstream-to=origin/"), "{observed_spec}: {message}");
        }
        Ok(())
    }

    #[test]
    fn detect_10_only_the_moved_clone_of_a_shared_url_moves() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1 b=u1@2", "a=u1@1 x=u1@2")?, vec![fixtures::move_to("b", "x", "u1")?]);
        Ok(())
    }

    #[test]
    fn detect_11_blocked_and_still_on_disk() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1!", "a=u1@1")?, vec![]);
        Ok(())
    }

    #[test]
    fn detect_12_blocked_and_deleted() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1!", "")?, vec![fixtures::remove("a", "u1")?]);
        Ok(())
    }

    #[test]
    fn detect_matches_by_inode_before_path() -> anyhow::Result<()> {
        assert_eq!(changes("a=u1@1", "a=u1@9 b=u1@1")?, vec![
            fixtures::move_to("a", "b", "u1")?,
            fixtures::add("a", "u1")?,
        ]);
        Ok(())
    }

    fn record(snapshot: &str, local: anyhow::Result<Vec<layout::Change>>) -> anyhow::Result<Recorded> {
        Ok(apply_to_snapshot(&fixtures::layout(snapshot)?, &local?))
    }

    fn recorded_layout(snapshot: &str, local: anyhow::Result<Vec<layout::Change>>) -> anyhow::Result<layout::Layout> {
        match record(snapshot, local)? {
            Recorded::Changed { snapshot, .. } => Ok(snapshot),
            other @ (Recorded::Unchanged | Recorded::Conflicts(_)) => {
                anyhow::bail!("expected a change, got {other:?}")
            }
        }
    }

    fn conflicted(snapshot: &str, local: anyhow::Result<Vec<layout::Change>>) -> anyhow::Result<Vec<LocalConflict>> {
        match record(snapshot, local)? {
            Recorded::Conflicts(conflicts) => Ok(conflicts),
            other @ (Recorded::Unchanged | Recorded::Changed { .. }) => {
                anyhow::bail!("expected conflicts, got {other:?}")
            }
        }
    }

    #[test]
    fn apply_add_of_a_repo_the_snapshot_already_has_is_nothing() -> anyhow::Result<()> {
        assert_eq!(record("a=u1", fixtures::add("a", "u1").map(|c| vec![c]))?, Recorded::Unchanged);
        Ok(())
    }

    #[test]
    fn apply_add_over_another_url_is_a_local_conflict() -> anyhow::Result<()> {
        let conflicts = conflicted("a=u2", fixtures::add("a", "u1").map(|c| vec![c]))?;
        let message = conflicts.first().map(|conflict| conflict.message.clone()).unwrap_or_default();
        assert!(message.contains("a is in the layout as u2, but the clone on disk has origin u1"), "{message}");
        Ok(())
    }

    #[test]
    fn apply_add_of_a_new_path_adds_it() -> anyhow::Result<()> {
        assert_eq!(recorded_layout("b=u2", fixtures::add("a", "u1").map(|c| vec![c]))?, fixtures::layout("a=u1 b=u2")?);
        Ok(())
    }

    #[test]
    fn apply_add_nested_in_the_snapshot_is_a_local_conflict() -> anyhow::Result<()> {
        let conflicts = conflicted("a=u2", fixtures::add("a/b", "u1").map(|c| vec![c]))?;
        assert_eq!(conflicts.first().map(|conflict| conflict.path.clone()), Some(fixtures::path("a/b")?));
        Ok(())
    }

    #[test]
    fn apply_remove_of_a_present_path_removes_it() -> anyhow::Result<()> {
        let recorded = recorded_layout("a=u1 b=u1", fixtures::remove("a", "u1").map(|c| vec![c]))?;
        assert_eq!(recorded, fixtures::layout("b=u1")?);
        Ok(())
    }

    #[test]
    fn apply_remove_of_a_clone_the_snapshot_replaced_is_nothing() -> anyhow::Result<()> {
        assert_eq!(record("a=u2", fixtures::remove("a", "u1").map(|c| vec![c]))?, Recorded::Unchanged);
        Ok(())
    }

    #[test]
    fn apply_move_of_a_clone_the_snapshot_replaced_adds_the_destination_only() -> anyhow::Result<()> {
        let recorded = recorded_layout("a=u2", fixtures::move_to("a", "b", "u1").map(|c| vec![c]))?;
        assert_eq!(recorded, fixtures::layout("a=u2 b=u1")?);
        Ok(())
    }

    #[test]
    fn apply_remove_of_an_absent_path_is_nothing() -> anyhow::Result<()> {
        assert_eq!(record("b=u1", fixtures::remove("a", "u1").map(|c| vec![c]))?, Recorded::Unchanged);
        Ok(())
    }

    #[test]
    fn apply_move_to_a_free_path_moves() -> anyhow::Result<()> {
        let recorded = recorded_layout("a=u1", fixtures::move_to("a", "b", "u1").map(|c| vec![c]))?;
        assert_eq!(recorded, fixtures::layout("b=u1")?);
        Ok(())
    }

    #[test]
    fn apply_swap_moves_both() -> anyhow::Result<()> {
        let swap = fixtures::move_to("a", "b", "u1")
            .and_then(|first| Ok(vec![first, fixtures::move_to("b", "a", "u2")?]));
        assert_eq!(recorded_layout("a=u1 b=u2", swap)?, fixtures::layout("a=u2 b=u1")?);
        Ok(())
    }

    #[test]
    fn apply_move_of_a_path_the_snapshot_lacks_adds_the_destination() -> anyhow::Result<()> {
        assert_eq!(recorded_layout("", fixtures::move_to("a", "b", "u1").map(|c| vec![c]))?, fixtures::layout("b=u1")?);
        Ok(())
    }

    #[test]
    fn apply_move_onto_the_same_repo_removes_the_source() -> anyhow::Result<()> {
        let recorded = recorded_layout("a=u1 b=u1", fixtures::move_to("a", "b", "u1").map(|c| vec![c]))?;
        assert_eq!(recorded, fixtures::layout("b=u1")?);
        assert_eq!(record("b=u1", fixtures::move_to("a", "b", "u1").map(|c| vec![c]))?, Recorded::Unchanged);
        Ok(())
    }

    #[test]
    fn apply_move_onto_another_repo_is_a_local_conflict() -> anyhow::Result<()> {
        let conflicts = conflicted("a=u1 b=u2", fixtures::move_to("a", "b", "u1").map(|c| vec![c]))?;
        assert_eq!(conflicts.first().map(|conflict| conflict.path.clone()), Some(fixtures::path("b")?));
        Ok(())
    }

    #[test]
    fn apply_set_url_of_a_present_path_sets_it() -> anyhow::Result<()> {
        let recorded = recorded_layout("a=u1", fixtures::set_url("a", "u1", "u2").map(|c| vec![c]))?;
        assert_eq!(recorded, fixtures::layout("a=u2")?);
        Ok(())
    }

    #[test]
    fn apply_set_url_of_an_absent_path_is_nothing() -> anyhow::Result<()> {
        assert_eq!(record("b=u1", fixtures::set_url("a", "u1", "u2").map(|c| vec![c]))?, Recorded::Unchanged);
        Ok(())
    }

    #[test]
    fn apply_a_fresh_clone_where_a_moved_repo_used_to_be() -> anyhow::Result<()> {
        let local = fixtures::add("a", "u1").and_then(|add| Ok(vec![add, fixtures::move_to("a", "b", "u1")?]));
        assert_eq!(recorded_layout("a=u1", local)?, fixtures::layout("a=u1 b=u1")?);
        Ok(())
    }

    #[test]
    fn next_base_keeps_synced_blocked_and_failed_removals_only() -> anyhow::Result<()> {
        let snapshot = fixtures::layout("a=u1 b=u2 p=u3")?;
        let old = base("a=u1@1 gone=u4@4 blocked=u5@5! failed=u6@6 newly=u7@7")?;
        let disk = observed("a=u1@11 b=u2@12 blocked=u5@5 failed=u6@6 newly=u7@7 local=-@8 extra=u9@9")?;
        let newly_blocked = BTreeSet::from([fixtures::path("newly")?]);
        let next = next_base(&snapshot, &old, &disk, &newly_blocked);
        let expected = base("a=u1@11 b=u2@12 blocked=u5@5! failed=u6@6 newly=u7@7!")?;
        assert_eq!(next, expected);
        Ok(())
    }

    #[test]
    fn commit_message_summarizes_and_lists_every_change() -> anyhow::Result<()> {
        let host = "laptop".parse::<domain::HostName>()?;
        let single = commit_message(&host, &[fixtures::add("ferrisoft/x", "git@github.com:ferrisoft/x.git")?]);
        assert_eq!(single.subject, "laptop: +ferrisoft/x");
        assert_eq!(single.body, "add ferrisoft/x (git@github.com:ferrisoft/x.git)");
        let many = (0..12)
            .map(|n| fixtures::add(&format!("some/longer/path-{n}"), "u"))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let message = commit_message(&host, &many);
        assert_eq!(message.subject, "laptop: +some/longer/path-0, +some/longer/path-1, +some/longer/path-2 (+9 more)");
        assert_eq!(message.body.lines().count(), 12);
        let mixed = [
            fixtures::remove("a", "u1")?,
            fixtures::move_to("b", "c", "u2")?,
            fixtures::set_url("d", "u3", "u4")?,
        ];
        let message = commit_message(&host, &mixed);
        assert_eq!(message.subject, "laptop: -a, b → c, d: u4");
        assert_eq!(message.body, "remove a (u1)\nmove b → c (u2)\nset origin of d: u3 → u4");
        Ok(())
    }
}
