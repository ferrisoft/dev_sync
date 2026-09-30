//! Carries out a reconcile plan on disk (§8.8).

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::domain;
use crate::git;
use crate::layout;
use crate::parallel;
use crate::reconcile::facts;
use crate::reconcile::plan;
use crate::record;
use crate::report;
use crate::safety;
use crate::scan;
use crate::shell;
use crate::state;


// ================
// === Executed ===
// ================

#[derive(Debug)]
#[must_use]
pub(crate) struct Executed {
    pub(crate) report: report::Report,
    /// The base to save: rebuilt from a scan after the changes. `None` when that scan failed; then the base stays as
    /// it was, and the next run catches up like after a crash.
    pub(crate) state: Option<state::MachineState>,
}

/// Runs the plan: unfinished clones from an interrupted run are deleted, then removals (after the safety check),
/// moves (in two phases, so chains and swaps work), url changes, and finally clones in parallel. One repo's failure
/// never stops the others; everything lands in the report, and failed steps are retried by the next pull.
pub(crate) fn execute(
    git: &git::Git,
    root: &Path,
    target: &layout::Layout,
    plan: &plan::Plan,
    base: &state::MachineState,
    leftovers: &[scan::Leftover],
) -> Executed {
    let mut report = report::Report::default();
    remove_unfinished_clones(leftovers, &mut report);
    for conflict in &plan.conflicts {
        report.attention(report::Scope::Disk, conflict.to_string());
    }
    let departing = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            plan::Action::Remove { path } | plan::Action::Move { from: path, .. } => Some(path.clone()),
            plan::Action::Adopt { .. } | plan::Action::SetUrl { .. } | plan::Action::Clone { .. } => None,
        })
        .collect::<BTreeSet<_>>();
    let mut blocked = BTreeSet::new();
    for action in &plan.actions {
        if let plan::Action::Remove { path } = action {
            blocked.extend(remove(git, root, path, &mut report));
        }
    }
    let moves = plan.actions.iter().filter_map(|action| match action {
        plan::Action::Move { from, to } => Some(plan::Relocation { from: from.clone(), to: to.clone() }),
        _ => None,
    });
    relocate(git, root, moves.collect(), &departing, &mut report);
    for action in &plan.actions {
        if let plan::Action::SetUrl { path, url } = action {
            set_url(git, root, path, url, &mut report);
        }
    }
    let clones = plan
        .actions
        .iter()
        .filter_map(|action| match action {
            plan::Action::Clone { path, url } => Some(layout::LayoutRepo { path: path.clone(), url: url.clone() }),
            _ => None,
        })
        .collect::<Vec<_>>();
    let clones = clones_with_room(root, clones, &departing, &mut report);
    match parallel::map(&clones, git.policy().parallelism, |repo| clone(git, root, repo)) {
        Ok(cloned) => cloned.into_iter().for_each(|clone_report| report.extend(clone_report)),
        Err(error) => report.failure(report::Scope::Disk, format!("cloning stopped: {error:#}")),
    }
    for action in &plan.actions {
        if let plan::Action::Adopt { path } = action {
            report.info(report::Scope::Disk, format!("found {path} already cloned; it is tracked now"));
        }
    }
    let state = match scan::scan(git, root) {
        Ok(scanned) => Some(record::next_base(target, base, &scanned.repos, &blocked)),
        Err(error) => {
            let message = format!(
                "failed to look at the workspace after these changes, so this machine's base wasn't updated (the \
                 next run catches up): {error:#}"
            );
            report.failure(report::Scope::Disk, message);
            None
        }
    };
    Executed { report, state }
}


// ================
// === Removals ===
// ================

/// Trashes the clone at `path` if nothing in it exists only here. Returns the path when the removal is blocked.
fn remove(
    git: &git::Git,
    root: &Path,
    path: &domain::RepoPath,
    report: &mut report::Report,
) -> Option<domain::RepoPath> {
    let dir = path.to_fs_path(root);
    let keep = shell::word(path.as_str());
    match check_removal(git, &dir) {
        Err(error) => {
            report.failure(report::Scope::Disk, format!("couldn't check {path} before removing it: {error:#}"));
            None
        }
        Ok(RemovalCheck::Gone) => None,
        Ok(RemovalCheck::Unverified(failure)) => {
            report.attention(
                report::Scope::Disk,
                format!(
                    "kept {path}, which was removed from the layout: {} — the next pull checks again; if its remote \
                     is gone on purpose, move {path} to the Trash yourself",
                    failure.describe("checking it against its remote")
                ),
            );
            None
        }
        Ok(RemovalCheck::OtherFilesystem { trash }) => {
            report.attention(
                report::Scope::Disk,
                format!(
                    "{path} was removed from the layout, but it is on another filesystem than the Trash ({}) and \
                     dev_sync never copies repositories — move it to the Trash yourself, or run `dev_sync keep {keep}`",
                    trash.display()
                ),
            );
            Some(path.clone())
        }
        Ok(RemovalCheck::Unsafe(reasons)) => {
            let reasons = reasons.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            report.attention(
                report::Scope::Disk,
                format!(
                    "{path} was removed from the layout, but it holds work that exists only here: {reasons} — push \
                     or discard the work, then run `dev_sync pull` again; or run `dev_sync keep {keep}` to put it \
                     back in the layout"
                ),
            );
            Some(path.clone())
        }
        Ok(RemovalCheck::Safe) => {
            match trash::delete(&dir) {
                Ok(()) => {
                    report.done(report::Scope::Disk, format!("removed {path} (moved to the Trash)"));
                    prune_empty_parents(root, path);
                }
                Err(error) => {
                    report.failure(report::Scope::Disk, format!("failed to move {path} to the Trash: {error}"));
                }
            }
            None
        }
    }
}

enum RemovalCheck {
    Gone,
    /// Its remote couldn't be fetched, so commits that exist only here can't be told apart.
    Unverified(git::RemoteFailure),
    Unsafe(Vec<safety::UnsafeReason>),
    /// Trashing it would copy it (see `same_filesystem`).
    OtherFilesystem { trash: PathBuf },
    Safe,
}

/// Fetches the clone first: remote-tracking refs from an old fetch can cover commits whose branch has since been
/// deleted or force-pushed on the remote, which would make local-only work look pushed.
fn check_removal(git: &git::Git, dir: &Path) -> anyhow::Result<RemovalCheck> {
    let is_repo = facts::metadata(&dir.join(".git"))?.is_some_and(|found| found.is_dir());
    let trash = trash_directory();
    Ok(match (facts::metadata(dir)?, is_repo) {
        (None, _) => RemovalCheck::Gone,
        (Some(_), false) => RemovalCheck::Unsafe(vec![safety::UnsafeReason::NotARepository]),
        (Some(_), true) => {
            let fetched = git.at(dir).args(["fetch", "--all", "--prune"]).remote(git::Prompts::Allowed)?;
            match fetched {
                git::RemoteOutcome::Failed(failure) => RemovalCheck::Unverified(failure),
                git::RemoteOutcome::Succeeded(_) => match safety::removal_safety(git, dir)? {
                    safety::RemovalSafety::Unsafe(reasons) => RemovalCheck::Unsafe(reasons),
                    safety::RemovalSafety::Safe => match trash {
                        Some(trash) if !same_filesystem(dir, &trash)? => RemovalCheck::OtherFilesystem { trash },
                        Some(_) | None => RemovalCheck::Safe,
                    },
                },
            }
        }
    })
}

/// The home Trash of the freedesktop spec: `$XDG_DATA_HOME/Trash`, or `~/.local/share/Trash`.
fn trash_directory() -> Option<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).filter(|dir| dir.is_absolute());
    let data_home = data_home.or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    data_home.map(|dir| dir.join("Trash"))
}

/// Whether `path` and `other` (or its nearest existing ancestor) live on one filesystem, so a move between them is a
/// rename. Across filesystems the trash crate copies the whole repository and then deletes it, which can take long
/// and leaves half a repository behind when interrupted.
fn same_filesystem(path: &Path, other: &Path) -> anyhow::Result<bool> {
    let device = |path: &Path| -> anyhow::Result<u64> {
        let existing = path.ancestors().find(|ancestor| ancestor.exists()).unwrap_or(path);
        Ok(std::fs::metadata(existing).with_context(|| format!("failed to inspect {}", existing.display()))?.dev())
    };
    Ok(device(path)? == device(other)?)
}

/// Removes the now-empty directories above `path`, nearest first, stopping at the first one that isn't empty. Never
/// the root, never a hidden directory (a repository path has no hidden components).
fn prune_empty_parents(root: &Path, path: &domain::RepoPath) {
    for ancestor in path.proper_ancestors().iter().rev() {
        if std::fs::remove_dir(ancestor.to_fs_path(root)).is_err() {
            break;
        }
    }
}


// =============
// === Moves ===
// =============

/// A move under way: between leaving its source and landing, the repository is parked at `temp`.
struct Job {
    to: domain::RepoPath,
    needs: BTreeSet<domain::RepoPath>,
    temp: PathBuf,
    spot: Spot,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Spot {
    Parked,
    Landed,
    /// Back at its source: the move didn't happen.
    Returned,
    /// Still parked, because putting it back failed.
    Stranded,
}

/// Why a parked repository goes back to its source.
enum Setback {
    Blocked(plan::Blocker),
    Failed(anyhow::Error),
}

/// Moves in two phases, so chains and swaps work: every source is parked under a temporary name in the root (on the
/// same filesystem, so these are renames), then each repository lands at its destination. Moves that can't land are
/// dropped before anything moves (see `plan::landing`), judged by the disk as the removals left it. Repositories land
/// from the free end of each chain, so one that fails finds its source free; in a cycle, the moves that already
/// landed there are lifted back first. Only when putting a repository back fails too does it stay parked, and the
/// report then names the directory, which is never deleted.
fn relocate(
    git: &git::Git,
    root: &Path,
    moves: Vec<plan::Relocation>,
    departing: &BTreeSet<domain::RepoPath>,
    report: &mut report::Report,
) {
    let destinations = moves.iter().map(|relocation| relocation.to.clone()).collect::<Vec<_>>();
    match facts::collect_facts(root, destinations.iter()) {
        Err(error) => report.failure(
            report::Scope::Disk,
            format!("failed to look at the places repositories move to, so none moved: {error:#}"),
        ),
        Ok(facts) => {
            let landing = plan::landing(moves, Vec::new(), &BTreeSet::new(), departing, &facts);
            for conflict in &landing.conflicts {
                report.attention(report::Scope::Disk, conflict.to_string());
            }
            let mut jobs = park(root, landing.moves, report);
            land_all(root, &mut jobs, report);
            for (from, job) in jobs.iter().filter(|(_, job)| job.spot == Spot::Landed) {
                report.done(report::Scope::Disk, format!("moved {from} → {}", job.to));
                repair_worktrees(git, &from.to_fs_path(root), &job.to.to_fs_path(root), &job.to, report);
                prune_empty_parents(root, from);
            }
        }
    }
}

/// Parks every source under a temporary name in the root.
fn park(
    root: &Path,
    placements: Vec<plan::Placement>,
    report: &mut report::Report,
) -> BTreeMap<domain::RepoPath, Job> {
    let pid = std::process::id();
    placements
        .into_iter()
        .zip(0_usize..)
        .filter_map(|(placement, index)| {
            let temp = root.join(format!("{}{pid}-{index}", scan::MOVING_PREFIX));
            let plan::Placement { from, to, needs } = placement;
            match std::fs::rename(from.to_fs_path(root), &temp) {
                Ok(()) => Some((from, Job { to, needs, temp, spot: Spot::Parked })),
                Err(error) => {
                    let message = format!("failed to move {from} → {to}: {}", io_problem(&error));
                    report.failure(report::Scope::Disk, message);
                    None
                }
            }
        })
        .collect()
}

/// Lands each parked repository, or puts it back when a move it waits for didn't happen (its source failed to park,
/// or went back) or its destination can't take it.
fn land_all(root: &Path, jobs: &mut BTreeMap<domain::RepoPath, Job>, report: &mut report::Report) {
    let needs = jobs.iter().map(|(from, job)| (from.clone(), job.needs.clone())).collect();
    let order = landing_order(&needs);
    tracing::debug!(order = ?order, "landing parked repositories");
    for from in order {
        let step = jobs.get(&from).filter(|job| job.spot == Spot::Parked).map(|job| {
            let gave_up = |need: &&domain::RepoPath| {
                **need != from
                    && jobs.get(*need).is_none_or(|other| matches!(other.spot, Spot::Returned | Spot::Stranded))
            };
            match job.needs.iter().find(gave_up) {
                Some(need) => Err(Setback::Blocked(plan::Blocker::Staying(need.clone()))),
                None => land(root, &job.to, &job.temp),
            }
        });
        match step {
            None => {}
            Some(Ok(())) => set_spot(jobs, &from, Spot::Landed),
            Some(Err(setback)) => retreat(root, jobs, &from, setback, report),
        }
    }
}

/// Moves land after the moves whose sources they need gone, so a move that fails finds its source free. A cycle has
/// no such order; its first move by path goes first.
fn landing_order(needs: &BTreeMap<domain::RepoPath, BTreeSet<domain::RepoPath>>) -> Vec<domain::RepoPath> {
    let mut remaining = needs.keys().cloned().collect::<BTreeSet<_>>();
    let mut order = Vec::with_capacity(remaining.len());
    while let Some(next) = next_to_land(needs, &remaining) {
        remaining.remove(&next);
        order.push(next);
    }
    order
}

fn next_to_land(
    needs: &BTreeMap<domain::RepoPath, BTreeSet<domain::RepoPath>>,
    remaining: &BTreeSet<domain::RepoPath>,
) -> Option<domain::RepoPath> {
    let ready = remaining.iter().find(|from| {
        needs.get(*from).is_none_or(|wanted| wanted.iter().all(|need| need == *from || !remaining.contains(need)))
    });
    ready.or_else(|| remaining.first()).cloned()
}

fn land(root: &Path, to: &domain::RepoPath, temp: &Path) -> Result<(), Setback> {
    let destination = to.to_fs_path(root);
    match make_room(root, to) {
        Ok(Room::Free) => destination
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(temp, &destination))
            .map_err(|error| Setback::Failed(anyhow::anyhow!(io_problem(&error)))),
        Ok(Room::Taken(blocker)) => Err(Setback::Blocked(blocker)),
        Err(error) => Err(Setback::Failed(error)),
    }
}

/// Puts the repository parked for the move from `from` back at its source. Moves that already landed on or around
/// that source, which only happens within a cycle, are lifted back into their parking spots first and then go back
/// to their own sources.
fn retreat(
    root: &Path,
    jobs: &mut BTreeMap<domain::RepoPath, Job>,
    from: &domain::RepoPath,
    setback: Setback,
    report: &mut report::Report,
) {
    let occupants = jobs
        .iter()
        .filter(|(_, job)| job.spot == Spot::Landed && job.needs.contains(from))
        .map(|(occupant, _)| occupant.clone())
        .collect::<Vec<_>>();
    let mut lifted = Vec::new();
    let mut stuck = None;
    for occupant in occupants {
        if let Some(job) = jobs.get_mut(&occupant) {
            match std::fs::rename(job.to.to_fs_path(root), &job.temp) {
                Ok(()) => {
                    job.spot = Spot::Parked;
                    lifted.push(occupant);
                }
                Err(error) => stuck = Some(format!("{} couldn't make way: {}", job.to, io_problem(&error))),
            }
        }
    }
    if let Some(job) = jobs.get(from) {
        let back = match stuck {
            Some(problem) => Err(problem),
            None => put_back(root, from, &job.temp).map_err(|error| io_problem(&error)),
        };
        let spot = report_retreat(from, job, setback, back, report);
        set_spot(jobs, from, spot);
    }
    for occupant in lifted {
        retreat(root, jobs, &occupant, Setback::Blocked(plan::Blocker::Staying(from.clone())), report);
    }
}

fn put_back(root: &Path, from: &domain::RepoPath, temp: &Path) -> std::io::Result<()> {
    let source = from.to_fs_path(root);
    source.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::rename(temp, &source))
}

fn report_retreat(
    from: &domain::RepoPath,
    job: &Job,
    setback: Setback,
    back: Result<(), String>,
    report: &mut report::Report,
) -> Spot {
    let to = &job.to;
    match (back, setback) {
        (Ok(()), Setback::Blocked(blocker)) => {
            let conflict = plan::DiskConflict { path: to.clone(), from: Some(from.clone()), blocker };
            report.attention(report::Scope::Disk, conflict.to_string());
            Spot::Returned
        }
        (Ok(()), Setback::Failed(error)) => {
            report.failure(report::Scope::Disk, format!("failed to move {from} → {to}: {error:#}; left it at {from}"));
            Spot::Returned
        }
        (Err(problem), setback) => {
            let why = match setback {
                Setback::Blocked(blocker) => blocker.to_string(),
                Setback::Failed(error) => format!("{error:#}"),
            };
            report.failure(
                report::Scope::Disk,
                format!(
                    "failed to move {from} → {to} ({why}), and to put it back ({problem}); the repository is in {} — \
                     move it by hand",
                    job.temp.display()
                ),
            );
            Spot::Stranded
        }
    }
}

fn set_spot(jobs: &mut BTreeMap<domain::RepoPath, Job>, from: &domain::RepoPath, spot: Spot) {
    if let Some(job) = jobs.get_mut(from) {
        job.spot = spot;
    }
}

/// Points linked worktrees and their repository back at each other after a move. Each
/// `.git/worktrees/<name>/gitdir` holds the absolute path of a worktree's `.git` file; worktrees that lived inside
/// the old location moved along, and `git worktree repair` is told where they are now. Worktrees outside get fixed
/// by `repair` without arguments.
fn repair_worktrees(git: &git::Git, old: &Path, new: &Path, path: &domain::RepoPath, report: &mut report::Report) {
    let admin = new.join(".git").join("worktrees");
    if let Ok(entries) = std::fs::read_dir(&admin) {
        let moved = entries
            .filter_map(Result::ok)
            .filter_map(|entry| std::fs::read(entry.path().join("gitdir")).ok())
            .filter_map(|bytes| {
                let recorded = Path::new(OsStr::from_bytes(bytes.strip_suffix(b"\n").unwrap_or(&bytes)));
                let git_file = new.join(recorded.strip_prefix(old).ok()?);
                git_file.parent().map(Path::to_path_buf)
            })
            .collect::<Vec<_>>();
        if let Err(error) = git.at(new).args(["worktree", "repair"]).args(&moved).run_ok(git::Access::Write) {
            report.attention(
                report::Scope::Disk,
                format!(
                    "moved {path}, but repairing its linked worktrees failed: {error:#} — run `git -C {} worktree \
                     repair` by hand",
                    shell::word(&new.to_string_lossy())
                ),
            );
        }
    }
}

fn io_problem(error: &std::io::Error) -> String {
    match error.kind() {
        ErrorKind::CrossesDevices => "it would cross filesystems, and dev_sync never copies repositories".to_owned(),
        _ => error.to_string(),
    }
}


// ===============
// === set_url ===
// ===============

/// Points the clone at `url`, but only once `url` turned out to hold the same history: a new URL usually means a
/// moved repository, but it can also be another repository placed at the same path, and repointing an unrelated
/// clone would mix two histories.
fn set_url(
    git: &git::Git,
    root: &Path,
    path: &domain::RepoPath,
    url: &domain::RemoteUrl,
    report: &mut report::Report,
) {
    let dir = path.to_fs_path(root);
    let changed = git::probe_history(git, &dir, url).and_then(|history| match history {
        git::History::Shared => git
            .at(&dir)
            .args(["remote", "set-url", "--", "origin", url.as_str()])
            .run_ok(git::Access::Write)
            .map(|_| git::History::Shared),
        other @ (git::History::Unrelated | git::History::Unreachable(_)) => Ok(other),
    });
    match changed {
        Ok(git::History::Shared) => report.done(report::Scope::Disk, format!("set the origin of {path} to {url}")),
        Ok(git::History::Unrelated) => report.attention(
            report::Scope::Disk,
            format!(
                "the layout now names {url} for {path}, which shares no history with the clone there — move the \
                 clone out of the workspace (or into a hidden folder), then `dev_sync pull` clones {url} in its place"
            ),
        ),
        Ok(git::History::Unreachable(failure)) => report.attention(
            report::Scope::Disk,
            format!(
                "kept the origin of {path}: {} — the next pull checks again",
                failure.describe(&format!("checking {url} before switching to it"))
            ),
        ),
        Err(error) => report.failure(report::Scope::Disk, format!("failed to set the origin of {path}: {error:#}")),
    }
}


// ==============
// === Clones ===
// ==============

enum Cloned {
    Done,
    Taken(plan::Blocker),
    Failed(git::RemoteFailure),
}

/// The clones whose destination can take a repository now that removals and moves are done.
fn clones_with_room(
    root: &Path,
    clones: Vec<layout::LayoutRepo>,
    departing: &BTreeSet<domain::RepoPath>,
    report: &mut report::Report,
) -> Vec<layout::LayoutRepo> {
    let destinations = clones.iter().map(|repo| repo.path.clone()).collect::<Vec<_>>();
    match facts::collect_facts(root, destinations.iter()) {
        Err(error) => {
            let message = format!("failed to look at the places repositories are cloned to, so none was: {error:#}");
            report.failure(report::Scope::Disk, message);
            Vec::new()
        }
        Ok(facts) => {
            let landing = plan::landing(Vec::new(), clones, &BTreeSet::new(), departing, &facts);
            for conflict in &landing.conflicts {
                report.attention(report::Scope::Disk, conflict.to_string());
            }
            landing.clones
        }
    }
}

/// Clones into a temporary directory next to the destination, then renames it into place, so an interrupted clone
/// never looks like a finished one.
fn clone(git: &git::Git, root: &Path, repo: &layout::LayoutRepo) -> report::Report {
    let mut report = report::Report::default();
    let path = &repo.path;
    match try_clone(git, root, repo) {
        Ok(Cloned::Done) => report.done(report::Scope::Disk, format!("cloned {path}")),
        Ok(Cloned::Taken(blocker)) => {
            let conflict = plan::DiskConflict { path: path.clone(), from: None, blocker };
            report.attention(report::Scope::Disk, conflict.to_string());
        }
        Ok(Cloned::Failed(failure)) => {
            report.failure(report::Scope::Disk, format!("{path}: {}", failure.describe("clone")));
        }
        Err(error) => report.failure(report::Scope::Disk, format!("failed to clone {path}: {error:#}")),
    }
    report
}

fn try_clone(git: &git::Git, root: &Path, repo: &layout::LayoutRepo) -> anyhow::Result<Cloned> {
    let destination = repo.path.to_fs_path(root);
    let parent = destination.parent().with_context(|| format!("{} has no parent", destination.display()))?;
    std::fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    let temp = parent.join(format!("{}{}-{}", scan::CLONING_PREFIX, repo.path.name(), std::process::id()));
    remove_temp(&temp)?;
    let outcome = git
        .in_directory(parent)
        .args(["clone", "--origin", "origin", "--", repo.url.as_str()])
        .arg(&temp)
        .remote_with_retry_prep(git::Prompts::Forbidden, || remove_temp(&temp));
    let settled = outcome.and_then(|outcome| match outcome {
        git::RemoteOutcome::Succeeded(_) => settle(root, &repo.path, &temp),
        git::RemoteOutcome::Failed(failure) => Ok(Cloned::Failed(failure)),
    });
    let cleaned = remove_temp(&temp);
    settled.and_then(|cloned| cleaned.map(|()| cloned))
}

fn settle(root: &Path, path: &domain::RepoPath, temp: &Path) -> anyhow::Result<Cloned> {
    match make_room(root, path)? {
        Room::Free => {
            let destination = path.to_fs_path(root);
            std::fs::rename(temp, &destination)
                .with_context(|| format!("failed to move the new clone into {}", destination.display()))?;
            Ok(Cloned::Done)
        }
        Room::Taken(blocker) => Ok(Cloned::Taken(blocker)),
    }
}

/// Deletes one of our own temporary clones, if it exists.
fn remove_temp(temp: &Path) -> anyhow::Result<()> {
    match std::fs::remove_dir_all(temp) {
        Err(error) if error.kind() != ErrorKind::NotFound => {
            Err(error).with_context(|| format!("failed to remove the temporary clone {}", temp.display()))
        }
        Ok(()) | Err(_) => Ok(()),
    }
}


// ============
// === Room ===
// ============

enum Room {
    Free,
    Taken(plan::Blocker),
}

/// Checks that `path` can take a repository: nothing is there but empty directories, which are removed, and no
/// repository is above it.
fn make_room(root: &Path, path: &domain::RepoPath) -> anyhow::Result<Room> {
    Ok(match facts::fact(root, path)? {
        facts::DestinationFact::Free => Room::Free,
        facts::DestinationFact::Directory { repos } => match repos.into_iter().next() {
            Some(repo) => Room::Taken(plan::Blocker::RepoInTheWay(repo)),
            None => match remove_empty_tree(&path.to_fs_path(root))? {
                true => Room::Free,
                false => Room::Taken(plan::Blocker::Obstacle(facts::Obstacle::NonEmptyDirectory)),
            },
        },
        facts::DestinationFact::Occupied { obstacle } => Room::Taken(plan::Blocker::Obstacle(obstacle)),
        facts::DestinationFact::InsideRepo { repo } => Room::Taken(plan::Blocker::InsideRepo(repo)),
    })
}

/// Removes a directory holding nothing but empty directories, deepest first. False when something else turns up, with
/// at most some of the empty directories removed.
fn remove_empty_tree(dir: &Path) -> anyhow::Result<bool> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("failed to read directory {}", dir.display()))?;
    let mut emptied = true;
    for entry in entries {
        let entry = entry.with_context(|| format!("failed to read directory {}", dir.display()))?;
        let kind = entry.file_type().with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        emptied = emptied && kind.is_dir() && remove_empty_tree(&entry.path())?;
    }
    match emptied.then(|| std::fs::remove_dir(dir)) {
        None => Ok(false),
        Some(Ok(())) => Ok(true),
        Some(Err(error)) if matches!(error.kind(), ErrorKind::DirectoryNotEmpty | ErrorKind::AlreadyExists) => {
            Ok(false)
        }
        Some(Err(error)) => {
            Err(error).with_context(|| format!("failed to remove the empty directory {}", dir.display()))
        }
    }
}


// =================
// === Leftovers ===
// =================

fn remove_unfinished_clones(leftovers: &[scan::Leftover], report: &mut report::Report) {
    for leftover in leftovers {
        if let scan::Leftover::Cloning(path) = leftover {
            match std::fs::remove_dir_all(path) {
                Ok(()) => report.info(
                    report::Scope::Disk,
                    format!("removed an unfinished clone left by an interrupted run: {}", path.display()),
                ),
                Err(error) => report.failure(
                    report::Scope::Disk,
                    format!("failed to remove the unfinished clone {}: {error}", path.display()),
                ),
            }
        }
    }
}

/// Puts back every repository an interrupted move left parked under a temporary name, as long as the base knows it
/// (by the id of its `.git`) and its old place is free. Without this, the next record would see the repository as
/// removed and spread the removal to every machine. A parked repository whose place is taken stops the command; one
/// the base doesn't know is reported, with its origin, and left alone.
pub(crate) fn restore_interrupted_moves(
    git: &git::Git,
    root: &Path,
    base: &state::MachineState,
    leftovers: &[scan::Leftover],
) -> anyhow::Result<report::Report> {
    let mut report = report::Report::default();
    for leftover in leftovers {
        if let scan::Leftover::Moving(temp) = leftover {
            let id = domain::FileId::of(&temp.join(".git")).ok();
            let owner = id.and_then(|id| base.repos.iter().find(|(_, known)| known.id == id));
            match owner {
                Some((path, known)) => {
                    let place = path.to_fs_path(root);
                    anyhow::ensure!(
                        facts::metadata(&place)?.is_none(),
                        "{} holds {path} (origin {}), parked by an interrupted move, but {path} is taken now — move \
                         it by hand to where repos.toml places it, then run `dev_sync pull` again",
                        temp.display(),
                        known.url,
                    );
                    place.parent().map_or(Ok(()), std::fs::create_dir_all)?;
                    std::fs::rename(temp, &place)
                        .with_context(|| format!("failed to move {} back to {}", temp.display(), place.display()))?;
                    report.done(report::Scope::Disk, format!("put {path} back after an interrupted move"));
                }
                None => report.attention(report::Scope::Disk, describe_unknown_parked(git, temp)),
            }
        }
    }
    Ok(report)
}

/// What an interrupted move left at `temp` that the base doesn't know: a clone, named by its origin so the user can
/// tell where it belongs, or no repository at all. A repository there may hold work that exists only here, so this
/// never suggests deleting it.
pub(crate) fn describe_unknown_parked(git: &git::Git, temp: &Path) -> String {
    let shown = temp.display();
    match std::fs::symlink_metadata(temp.join(".git")).is_ok() {
        false => format!(
            "{shown} was left behind by an interrupted move but holds no repository — look inside, then remove it"
        ),
        true => {
            let what = match git::origin(git, temp) {
                Ok(git::Origin::Url(url)) => format!("a clone of {url}"),
                Ok(git::Origin::Missing) => "a repository with no origin remote".to_owned(),
                Err(_) => "a repository".to_owned(),
            };
            format!(
                "{shown} holds {what}, left behind by an interrupted move — it may hold work that exists only here; \
                 move it where it belongs by hand"
            )
        }
    }
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::path::PathBuf;

    use anyhow::Context as _;

    use crate::domain;
    use crate::fixtures;
    use crate::git;
    use crate::layout;
    use crate::reconcile::facts;
    use crate::reconcile::plan;
    use crate::report;
    use crate::scan;
    use crate::state;
    use super::Executed;
    use super::Job;
    use super::Setback;
    use super::Spot;
    use super::clone;
    use super::execute;
    use super::landing_order;
    use super::prune_empty_parents;
    use super::relocate;
    use super::restore_interrupted_moves;
    use super::retreat;
    use super::same_filesystem;

    /// A repository at `path` with origin `url`.
    struct Entry<'a> {
        path: &'a str,
        url: &'a domain::RemoteUrl,
    }

    fn entry<'a>(path: &'a str, url: &'a domain::RemoteUrl) -> Entry<'a> {
        Entry { path, url }
    }

    struct World {
        sandbox: fixtures::Sandbox,
        root: PathBuf,
    }

    impl World {
        fn create() -> anyhow::Result<Self> {
            let sandbox = fixtures::Sandbox::create()?;
            let root = sandbox.path().join("workspace");
            std::fs::create_dir(&root)?;
            Ok(Self { root: root.canonicalize()?, sandbox })
        }

        fn remote(&self, name: &str) -> anyhow::Result<domain::RemoteUrl> {
            self.sandbox.remote(name)?.to_string_lossy().parse()
        }

        fn clone_at(&self, url: &domain::RemoteUrl, relative: &str) -> anyhow::Result<PathBuf> {
            self.sandbox.clone(Path::new(url.as_str()), &self.root.join(relative))
        }

        /// The base after a sync that left the given clones on disk.
        fn base(&self, entries: &[Entry<'_>]) -> anyhow::Result<state::MachineState> {
            let repos = entries
                .iter()
                .map(|Entry { path, url }| {
                    let id = domain::FileId::of(&self.root.join(path).join(".git"))?;
                    let known = state::KnownRepo { url: (*url).clone(), id, status: state::KnownStatus::Synced };
                    Ok((fixtures::path(path)?, known))
                })
                .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
            Ok(state::MachineState { repos })
        }

        fn origin(&self, relative: &str) -> anyhow::Result<String> {
            Ok(self.sandbox.git(&self.root.join(relative), &["remote", "get-url", "origin"])?.trim().to_owned())
        }

        /// Executes the plan; the rescan after it must succeed.
        fn run(&self, target: &layout::Layout, plan: plan::Plan, base: &state::MachineState) -> anyhow::Result<Ran> {
            let leftovers = scan::scan(&fixtures::git(), &self.root)?.leftovers;
            let Executed { report, state } = execute(&fixtures::git(), &self.root, target, &plan, base, &leftovers);
            let state = state.with_context(|| format!("the rescan failed:\n{}", report.render(false)))?;
            Ok(Ran { report, state })
        }
    }

    struct Ran {
        report: report::Report,
        state: state::MachineState,
    }

    fn layout_of(entries: &[Entry<'_>]) -> anyhow::Result<layout::Layout> {
        let repos = entries
            .iter()
            .map(|Entry { path, url }| Ok(layout::LayoutRepo { path: fixtures::path(path)?, url: (*url).clone() }))
            .collect::<anyhow::Result<Vec<_>>>()?;
        layout::Layout::from_repos(repos)
    }

    fn move_action(from: &str, to: &str) -> anyhow::Result<plan::Action> {
        Ok(plan::Action::Move { from: fixtures::path(from)?, to: fixtures::path(to)? })
    }

    fn temps_in(dir: &Path) -> anyhow::Result<Vec<String>> {
        Ok(std::fs::read_dir(dir)?
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".dev_sync-"))
            .collect())
    }

    #[test]
    fn a_move_carries_linked_worktrees_along() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        let old = world.clone_at(&url, "a")?;
        let linked = old.join(".claude").join("worktrees").join("x");
        world.sandbox.git(&old, &["worktree", "add", "--quiet", "-b", "x", &linked.to_string_lossy()])?;
        let base = world.base(&[entry("a", &url)])?;
        let target = layout_of(&[entry("b/c", &url)])?;
        let actions = vec![move_action("a", "b/c")?];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        assert_eq!(executed.report.status(), 0, "{}", executed.report.render(false));
        let new = world.root.join("b").join("c");
        assert!(!old.exists());
        let moved_along = new.join(".claude").join("worktrees").join("x");
        world.sandbox.git(&moved_along, &["status", "--short"])?;
        let listed = world.sandbox.git(&new, &["worktree", "list", "--porcelain"])?;
        assert!(listed.contains(&moved_along.to_string_lossy().to_string()), "{listed}");
        assert_eq!(executed.state.repos.keys().map(ToString::to_string).collect::<Vec<_>>(), vec!["b/c"]);
        Ok(())
    }

    #[test]
    fn a_swap_goes_through_temporary_names() -> anyhow::Result<()> {
        let world = World::create()?;
        let first = world.remote("first")?;
        let second = world.remote("second")?;
        world.clone_at(&first, "a")?;
        world.clone_at(&second, "b")?;
        let base = world.base(&[entry("a", &first), entry("b", &second)])?;
        let target = layout_of(&[entry("a", &second), entry("b", &first)])?;
        let actions = vec![move_action("a", "b")?, move_action("b", "a")?];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        assert_eq!(executed.report.status(), 0, "{}", executed.report.render(false));
        assert_eq!(world.origin("a")?, second.as_str());
        assert_eq!(world.origin("b")?, first.as_str());
        assert_eq!(temps_in(&world.root)?, Vec::<String>::new());
        assert_eq!(executed.state.repos.len(), 2);
        Ok(())
    }

    #[test]
    fn moving_away_prunes_empty_parents() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        world.clone_at(&url, "x/y/a")?;
        std::fs::create_dir_all(world.root.join("x").join("keep"))?;
        std::fs::write(world.root.join("x").join("keep").join("file"), "stays")?;
        let base = world.base(&[entry("x/y/a", &url)])?;
        let target = layout_of(&[entry("b", &url)])?;
        world.run(&target, plan::Plan { actions: vec![move_action("x/y/a", "b")?], conflicts: vec![] }, &base)?;
        assert!(!world.root.join("x").join("y").exists());
        assert!(world.root.join("x").join("keep").join("file").exists());
        Ok(())
    }

    #[test]
    fn a_blocked_destination_keeps_the_repo_in_place() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        world.clone_at(&url, "a")?;
        std::fs::create_dir_all(world.root.join("b"))?;
        std::fs::write(world.root.join("b").join("file"), "in the way")?;
        let base = world.base(&[entry("a", &url)])?;
        let target = layout_of(&[entry("b", &url)])?;
        let actions = vec![move_action("a", "b")?];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        assert_eq!(
            executed.report.render(false),
            "! can't move a → b: a non-empty directory is in the way — move it aside, then run `dev_sync pull` again\n"
        );
        assert!(world.root.join("a").join(".git").is_dir());
        assert_eq!(temps_in(&world.root)?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn a_blocked_removal_holds_back_the_moves_that_need_its_place() -> anyhow::Result<()> {
        let world = World::create()?;
        let (one, two, three) = (world.remote("one")?, world.remote("two")?, world.remote("three")?);
        world.clone_at(&one, "a")?;
        world.clone_at(&two, "b")?;
        let x = world.clone_at(&three, "x")?;
        world.sandbox.commit(&x, "work", "exists only here")?;
        let base = world.base(&[entry("a", &one), entry("b", &two), entry("x", &three)])?;
        let target = layout_of(&[entry("b", &one), entry("x/c", &two)])?;
        let actions =
            vec![plan::Action::Remove { path: fixtures::path("x")? }, move_action("a", "b")?, move_action("b", "x/c")?];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        let rendered = executed.report.render(false);
        assert_eq!(executed.report.status(), 2, "{rendered}");
        assert!(rendered.contains("can't move b → x/c: the repository at x is still there (see above)"), "{rendered}");
        assert!(rendered.contains("can't move a → b: the repository at b is still there (see above)"), "{rendered}");
        assert_eq!(world.origin("a")?, one.as_str());
        assert_eq!(world.origin("b")?, two.as_str());
        assert_eq!(temps_in(&world.root)?, Vec::<String>::new());
        assert_eq!(executed.state.repos.keys().map(ToString::to_string).collect::<Vec<_>>(), vec!["a", "b", "x"]);
        Ok(())
    }

    #[test]
    fn a_chain_whose_free_end_is_blocked_stays_put() -> anyhow::Result<()> {
        let world = World::create()?;
        let (one, two) = (world.remote("one")?, world.remote("two")?);
        world.clone_at(&one, "a")?;
        world.clone_at(&two, "b")?;
        std::fs::create_dir(world.root.join("c"))?;
        std::fs::write(world.root.join("c").join("file"), "in the way")?;
        let base = world.base(&[entry("a", &one), entry("b", &two)])?;
        let target = layout_of(&[entry("b", &one), entry("c", &two)])?;
        let actions = vec![move_action("a", "b")?, move_action("b", "c")?];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        let rendered = executed.report.render(false);
        assert_eq!(executed.report.status(), 2, "{rendered}");
        assert!(rendered.contains("can't move b → c: a non-empty directory is in the way"), "{rendered}");
        assert_eq!(world.origin("a")?, one.as_str());
        assert_eq!(world.origin("b")?, two.as_str());
        assert_eq!(temps_in(&world.root)?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn a_directory_holding_only_leaving_repos_takes_a_repo() -> anyhow::Result<()> {
        let world = World::create()?;
        let (one, two) = (world.remote("one")?, world.remote("two")?);
        world.clone_at(&one, "x/d/e")?;
        world.clone_at(&two, "y")?;
        let base = world.base(&[entry("x/d/e", &one), entry("y", &two)])?;
        let target = layout_of(&[entry("x", &two), entry("z", &one)])?;
        let actions = vec![move_action("x/d/e", "z")?, move_action("y", "x")?];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        assert_eq!(executed.report.status(), 0, "{}", executed.report.render(false));
        assert_eq!(world.origin("x")?, two.as_str());
        assert_eq!(world.origin("z")?, one.as_str());
        assert_eq!(executed.state.repos.keys().map(ToString::to_string).collect::<Vec<_>>(), vec!["x", "z"]);
        Ok(())
    }

    #[test]
    fn a_failed_rescan_keeps_the_report_of_what_was_done() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        world.clone_at(&url, "a")?;
        let base = world.base(&[entry("a", &url)])?;
        let sealed = world.root.join("sealed");
        std::fs::create_dir(&sealed)?;
        std::fs::set_permissions(&sealed, std::os::unix::fs::PermissionsExt::from_mode(0o000))?;
        let target = layout_of(&[entry("b", &url)])?;
        let the_plan = plan::Plan { actions: vec![move_action("a", "b")?], conflicts: vec![] };
        let executed = execute(&fixtures::git(), &world.root, &target, &the_plan, &base, &[]);
        std::fs::set_permissions(&sealed, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
        let rendered = executed.report.render(false);
        match rustix::process::geteuid().is_root() {
            true => assert!(executed.state.is_some(), "root reads every directory"),
            false => {
                assert!(rendered.contains("✓ moved a → b"), "{rendered}");
                assert!(rendered.contains("sealed"), "{rendered}");
                assert_eq!(executed.state, None);
            }
        }
        Ok(())
    }

    #[test]
    fn a_clone_lands_through_a_temporary_directory() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        let target = layout_of(&[entry("x/y", &url)])?;
        let actions = vec![plan::Action::Clone { path: fixtures::path("x/y")?, url: url.clone() }];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &state::MachineState::default())?;
        assert_eq!(executed.report.render(false), "✓ cloned x/y\n");
        assert_eq!(world.origin("x/y")?, url.as_str());
        assert_eq!(temps_in(&world.root.join("x"))?, Vec::<String>::new());
        assert_eq!(executed.state.repos.keys().map(ToString::to_string).collect::<Vec<_>>(), vec!["x/y"]);
        Ok(())
    }

    #[test]
    fn a_clone_always_names_its_remote_origin() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        let git = fixtures::git_configured("clone.defaultRemoteName", "upstream");
        let repo = layout::LayoutRepo { path: fixtures::path("a")?, url: url.clone() };
        let report = clone(&git, &world.root, &repo);
        assert_eq!(report.render(false), "✓ cloned a\n");
        assert_eq!(world.origin("a")?, url.as_str());
        Ok(())
    }

    #[test]
    fn a_clone_that_can_not_land_leaves_no_temporary_directory() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        std::fs::create_dir(world.root.join("z"))?;
        std::fs::write(world.root.join("z").join("file"), "in the way")?;
        let report = clone(&fixtures::git(), &world.root, &layout::LayoutRepo { path: fixtures::path("z")?, url });
        assert_eq!(report.status(), 2, "{}", report.render(false));
        assert_eq!(temps_in(&world.root)?, Vec::<String>::new());
        Ok(())
    }

    #[test]
    fn a_clone_of_a_missing_remote_is_reported_as_not_found() -> anyhow::Result<()> {
        let world = World::create()?;
        let missing = world.sandbox.path().join("no-such-remote").to_string_lossy().parse::<domain::RemoteUrl>()?;
        let target = layout_of(&[entry("z", &missing)])?;
        let actions = vec![plan::Action::Clone { path: fixtures::path("z")?, url: missing }];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &state::MachineState::default())?;
        assert_eq!(executed.report.status(), 1);
        let rendered = executed.report.render(false);
        assert!(rendered.contains("clone failed (not found)"), "{rendered}");
        assert!(!world.root.join("z").exists());
        assert_eq!(temps_in(&world.root)?, Vec::<String>::new());
        assert!(executed.state.repos.is_empty());
        Ok(())
    }

    #[test]
    fn sets_a_new_origin_url_of_the_same_repository() -> anyhow::Result<()> {
        let world = World::create()?;
        let old = world.remote("old")?;
        let new = world.sandbox.mirror(Path::new(old.as_str()), "new")?.to_string_lossy().parse::<domain::RemoteUrl>()?;
        world.clone_at(&old, "a")?;
        let base = world.base(&[entry("a", &old)])?;
        let target = layout_of(&[entry("a", &new)])?;
        let actions = vec![plan::Action::SetUrl { path: fixtures::path("a")?, url: new.clone() }];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        assert_eq!(world.origin("a")?, new.as_str());
        assert_eq!(executed.state.repos.get(&fixtures::path("a")?).map(|known| known.url.clone()), Some(new));
        assert_eq!(world.sandbox.git(&world.root.join("a"), &["for-each-ref", "refs/dev_sync"])?, "");
        Ok(())
    }

    #[test]
    fn keeps_the_origin_when_the_new_url_is_another_repository() -> anyhow::Result<()> {
        let world = World::create()?;
        let old = world.remote("old")?;
        let other = world.remote("other")?;
        world.clone_at(&old, "a")?;
        let base = world.base(&[entry("a", &old)])?;
        let target = layout_of(&[entry("a", &other)])?;
        let actions = vec![plan::Action::SetUrl { path: fixtures::path("a")?, url: other }];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        assert_eq!(executed.report.status(), 2, "{}", executed.report.render(false));
        assert_eq!(world.origin("a")?, old.as_str());
        assert_eq!(executed.state.repos.get(&fixtures::path("a")?).map(|known| known.url.clone()), Some(old));
        Ok(())
    }

    #[test]
    fn tells_whether_a_move_would_cross_filesystems() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        assert!(same_filesystem(dir.path(), &dir.path().join("not").join("there").join("yet"))?);
        assert!(!same_filesystem(dir.path(), Path::new("/proc/self"))?);
        Ok(())
    }

    #[test]
    fn cleans_up_unfinished_clones_but_never_parked_moves() -> anyhow::Result<()> {
        let world = World::create()?;
        let cloning = world.root.join("x").join(".dev_sync-cloning-y-1");
        std::fs::create_dir_all(cloning.join("objects"))?;
        let parked = world.root.join(".dev_sync-moving-1-0");
        std::fs::create_dir_all(&parked)?;
        let executed = world.run(&layout::Layout::default(), plan::Plan::default(), &state::MachineState::default())?;
        assert!(!cloning.exists());
        assert!(parked.exists());
        assert_eq!(executed.report.status(), 0, "{}", executed.report.render(false));
        Ok(())
    }

    #[test]
    fn puts_back_a_repo_parked_by_an_interrupted_move() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        world.clone_at(&url, "p/a")?;
        let base = world.base(&[entry("p/a", &url)])?;
        let parked = world.root.join(".dev_sync-moving-7-0");
        std::fs::rename(world.root.join("p").join("a"), &parked)?;
        std::fs::remove_dir(world.root.join("p"))?;
        let leftovers = scan::scan(&fixtures::git(), &world.root)?.leftovers;
        let report = restore_interrupted_moves(&fixtures::git(), &world.root, &base, &leftovers)?;
        assert_eq!(report.status(), 0, "{}", report.render(false));
        assert!(world.root.join("p").join("a").join(".git").is_dir());
        assert!(!parked.exists());
        Ok(())
    }

    #[test]
    fn a_parked_repo_whose_place_is_taken_stops_the_command() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        world.clone_at(&url, "a")?;
        let base = world.base(&[entry("a", &url)])?;
        let parked = world.root.join(".dev_sync-moving-7-0");
        std::fs::rename(world.root.join("a"), &parked)?;
        world.clone_at(&url, "a")?;
        let leftovers = scan::scan(&fixtures::git(), &world.root)?.leftovers;
        let restored = restore_interrupted_moves(&fixtures::git(), &world.root, &base, &leftovers);
        let message = restored.err().map(|e| format!("{e:#}"));
        assert!(message.unwrap_or_default().contains(".dev_sync-moving-7-0"));
        assert!(parked.exists());
        Ok(())
    }

    #[test]
    fn an_unknown_parked_repo_needs_attention() -> anyhow::Result<()> {
        let world = World::create()?;
        let url = world.remote("r")?;
        world.clone_at(&url, ".dev_sync-moving-3-1")?;
        let leftovers = scan::scan(&fixtures::git(), &world.root)?.leftovers;
        let base = state::MachineState::default();
        let report = restore_interrupted_moves(&fixtures::git(), &world.root, &base, &leftovers)?;
        let rendered = report.render(false);
        assert_eq!(report.status(), 2, "{rendered}");
        assert!(rendered.contains(&format!("holds a clone of {url}")), "{rendered}");
        assert!(!rendered.contains("delete"), "{rendered}");
        assert!(world.root.join(".dev_sync-moving-3-1").exists());
        Ok(())
    }

    const SPOTS: [&str; 10] = ["a", "b", "c", "a/x", "a/y", "b/z", "d/e", "d/f", "g", "d/e/h"];

    /// A small deterministic random source (xorshift).
    struct Dice(u64);

    impl Dice {
        fn below(&mut self, sides: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % sides.max(1)
        }
    }

    fn nested(one: &str, other: &str) -> bool {
        one == other || one.starts_with(&format!("{other}/")) || other.starts_with(&format!("{one}/"))
    }

    /// A fake repository: its origin and the id in its `.git`.
    #[derive(Debug)]
    struct FakeRepo {
        url: String,
        id: u64,
    }

    /// Repositories at random spots, never nested, by spot.
    fn scatter(dice: &mut Dice, ids: &mut u64) -> BTreeMap<&'static str, FakeRepo> {
        let mut repos = BTreeMap::<&'static str, FakeRepo>::new();
        for spot in SPOTS {
            if dice.below(100) < 45 && repos.keys().all(|taken| !nested(taken, spot)) {
                *ids += 1;
                repos.insert(spot, FakeRepo { url: format!("u{}", dice.below(3)), id: *ids });
            }
        }
        repos
    }

    /// Where each fake repository (by the id in its `.git`) is, and the temporary directories left behind.
    fn survey(
        root: &Path,
        dir: &Path,
        found: &mut BTreeMap<u64, String>,
        parked: &mut Vec<PathBuf>,
    ) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let name = path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
            match (name.starts_with(scan::MOVING_PREFIX), path.join(".git").is_dir()) {
                (true, _) => parked.push(path),
                (false, true) => {
                    let id = std::fs::read_to_string(path.join(".git").join("id"))?.parse()?;
                    found.insert(id, domain::RepoPath::from_fs_path(root, &path)?.to_string());
                }
                (false, false) if path.is_dir() => survey(root, &path, found, parked)?,
                (false, false) => {}
            }
        }
        Ok(())
    }

    #[test]
    fn moves_never_strand_or_lose_a_repository() -> anyhow::Result<()> {
        let mut dice = Dice(0x9E37_79B9_7F4A_7C15);
        let mut ids = 0;
        for round in 0..1500 {
            let dir = tempfile::tempdir()?;
            let root = dir.path();
            let (had, wanted) = (scatter(&mut dice, &mut ids), scatter(&mut dice, &mut ids));
            let mut known = BTreeMap::new();
            let mut observed = Vec::new();
            for (spot, FakeRepo { url, id }) in &had {
                std::fs::create_dir_all(root.join(spot).join(".git"))?;
                std::fs::write(root.join(spot).join(".git").join("id"), id.to_string())?;
                let path = fixtures::path(spot)?;
                let url = fixtures::url(url)?;
                let id = domain::FileId { device: 1, inode: *id };
                let status = state::KnownStatus::Synced;
                known.insert(path.clone(), state::KnownRepo { url: url.clone(), id, status });
                observed.push(scan::ObservedRepo { path, id, origin: git::Origin::Url(url) });
            }
            let wanted = wanted.iter().map(|(spot, FakeRepo { url, .. })| format!("{spot}={url}")).collect::<Vec<_>>();
            let target = fixtures::layout(&wanted.join(" "))?;
            let paths = target.repos().map(|repo| repo.path).collect::<Vec<_>>();
            let facts = facts::collect_facts(root, paths.iter())?;
            let the_plan = plan::plan(&state::MachineState { repos: known }, &target, &observed, &facts);
            let blocked = dice.below(3) == 0;
            let mut departing = BTreeSet::new();
            let mut moves = Vec::new();
            for action in &the_plan.actions {
                match action {
                    plan::Action::Remove { path } => {
                        departing.insert(path.clone());
                        if !blocked {
                            std::fs::remove_dir_all(path.to_fs_path(root))?;
                            prune_empty_parents(root, path);
                        }
                    }
                    plan::Action::Move { from, to } => {
                        departing.insert(from.clone());
                        moves.push(plan::Relocation { from: from.clone(), to: to.clone() });
                    }
                    plan::Action::Adopt { .. } | plan::Action::SetUrl { .. } | plan::Action::Clone { .. } => {}
                }
            }
            let mut report = report::Report::default();
            relocate(&fixtures::git(), root, moves.clone(), &departing, &mut report);
            let (mut found, mut parked) = (BTreeMap::new(), Vec::new());
            survey(root, root, &mut found, &mut parked)?;
            let context = format!("round {round}: had {had:?}, wanted {wanted:?}, blocked {blocked}\n{:?}", the_plan);
            assert_eq!(parked, Vec::<PathBuf>::new(), "{context}");
            assert_ne!(report.status(), 1, "{context}\n{}", report.render(false));
            for (spot, FakeRepo { url, id }) in &had {
                let destination = moves
                    .iter()
                    .find(|relocation| relocation.from.as_str() == *spot)
                    .map(|relocation| &relocation.to);
                let removed = departing.contains(&fixtures::path(spot)?) && destination.is_none();
                let now = found.get(id).map(String::as_str);
                let allowed = now == Some(spot)
                    || destination.is_some_and(|to| now == Some(to.as_str()))
                    || (removed && !blocked && now.is_none());
                assert!(allowed, "{context}\n{spot} ({url}) is now at {now:?}");
                if let (Some(to), Some(now)) = (destination, now)
                    && now == to.as_str()
                {
                    assert_eq!(target.get(to).map(|entry| entry.url.to_string()), Some(url.clone()), "{context}");
                }
                if the_plan.conflicts.is_empty() && !blocked && let Some(to) = destination {
                    assert_eq!(now, Some(to.as_str()), "{context}\n{}", report.render(false));
                }
            }
        }
        Ok(())
    }

    #[test]
    fn a_parked_directory_without_a_repository_says_so() -> anyhow::Result<()> {
        let world = World::create()?;
        std::fs::create_dir(world.root.join(".dev_sync-moving-3-2"))?;
        let leftovers = scan::scan(&fixtures::git(), &world.root)?.leftovers;
        let base = state::MachineState::default();
        let report = restore_interrupted_moves(&fixtures::git(), &world.root, &base, &leftovers)?;
        let rendered = report.render(false);
        assert!(rendered.contains("holds no repository"), "{rendered}");
        assert!(world.root.join(".dev_sync-moving-3-2").exists());
        Ok(())
    }

    /// A move from `from` that lands only once the repositories at `needs` are gone.
    struct Waiting<'a> {
        from: &'a str,
        needs: &'a [&'a str],
    }

    fn waits<'a>(from: &'a str, needs: &'a [&'a str]) -> Waiting<'a> {
        Waiting { from, needs }
    }

    fn needs(moves: &[Waiting<'_>]) -> anyhow::Result<BTreeMap<domain::RepoPath, BTreeSet<domain::RepoPath>>> {
        moves
            .iter()
            .map(|Waiting { from, needs: wanted }| {
                let wanted = wanted.iter().copied().map(fixtures::path).collect::<anyhow::Result<_>>()?;
                Ok((fixtures::path(from)?, wanted))
            })
            .collect()
    }

    fn order(moves: &[Waiting<'_>]) -> anyhow::Result<Vec<String>> {
        Ok(landing_order(&needs(moves)?).iter().map(ToString::to_string).collect())
    }

    #[test]
    fn lands_chains_from_the_free_end_and_starts_cycles_by_path() -> anyhow::Result<()> {
        assert_eq!(order(&[waits("a", &["b"]), waits("b", &["c"]), waits("c", &[])])?, vec!["c", "b", "a"]);
        assert_eq!(order(&[waits("a", &["b"]), waits("b", &["a"])])?, vec!["a", "b"]);
        let fan_in = [waits("x/c", &["y"]), waits("x/d", &[]), waits("y", &["x/c", "x/d"])];
        assert_eq!(order(&fan_in)?, vec!["x/d", "x/c", "y"]);
        assert_eq!(order(&[waits("a", &["a"]), waits("b", &[])])?, vec!["a", "b"]);
        Ok(())
    }

    #[test]
    fn a_failed_move_in_a_cycle_lifts_the_move_that_took_its_place() -> anyhow::Result<()> {
        let world = World::create()?;
        let (one, two) = (world.remote("one")?, world.remote("two")?);
        world.clone_at(&one, "a")?;
        world.clone_at(&two, "b")?;
        let (a, b) = (fixtures::path("a")?, fixtures::path("b")?);
        let (first, second) = (world.root.join(".dev_sync-moving-0-0"), world.root.join(".dev_sync-moving-0-1"));
        std::fs::rename(world.root.join("a"), &first)?;
        std::fs::rename(world.root.join("b"), &second)?;
        std::fs::rename(&first, world.root.join("b"))?;
        let mut jobs = BTreeMap::from([
            (a.clone(), Job { to: b.clone(), needs: BTreeSet::from([b.clone()]), temp: first, spot: Spot::Landed }),
            (b.clone(), Job { to: a.clone(), needs: BTreeSet::from([a]), temp: second, spot: Spot::Parked }),
        ]);
        let mut report = report::Report::default();
        retreat(&world.root, &mut jobs, &b, Setback::Failed(anyhow::anyhow!("the disk is full")), &mut report);
        assert_eq!(
            report.render(false),
            "✗ failed to move b → a: the disk is full; left it at b\n\
             ! can't move a → b: the repository at b is still there (see above)\n"
        );
        assert_eq!(world.origin("a")?, one.as_str());
        assert_eq!(world.origin("b")?, two.as_str());
        assert_eq!(temps_in(&world.root)?, Vec::<String>::new());
        assert!(jobs.values().all(|job| job.spot == Spot::Returned));
        Ok(())
    }

    #[test]
    fn a_move_that_can_not_start_holds_back_the_moves_waiting_for_it() -> anyhow::Result<()> {
        let world = World::create()?;
        let (one, two) = (world.remote("one")?, world.remote("two")?);
        world.clone_at(&one, "a")?;
        world.clone_at(&two, "b")?;
        let parking = world.root.join(format!("{}{}-1", scan::MOVING_PREFIX, std::process::id()));
        std::fs::create_dir(&parking)?;
        std::fs::write(parking.join("file"), "keeps b from being parked here")?;
        let base = world.base(&[entry("a", &one), entry("b", &two)])?;
        let target = layout_of(&[entry("b", &one), entry("c", &two)])?;
        let actions = vec![move_action("a", "b")?, move_action("b", "c")?];
        let executed = world.run(&target, plan::Plan { actions, conflicts: vec![] }, &base)?;
        let rendered = executed.report.render(false);
        assert_eq!(executed.report.status(), 1, "{rendered}");
        assert!(rendered.contains("✗ failed to move b → c: "), "{rendered}");
        assert!(rendered.contains("! can't move a → b: the repository at b is still there (see above)"), "{rendered}");
        assert_eq!(world.origin("a")?, one.as_str());
        assert_eq!(world.origin("b")?, two.as_str());
        Ok(())
    }
}
