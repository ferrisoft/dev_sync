//! What every changing command shares: the context, the preamble (§9.2), and the record and reconcile steps.

use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::content;
use crate::domain;
use crate::git;
use crate::layout;
use crate::reconcile;
use crate::record;
use crate::report;
use crate::scan;
use crate::shell;
use crate::state;
use crate::workspace;


// =====================
// === NOT_CONNECTED ===
// =====================

/// What to say when the workspace repository has no remote to sync with.
pub(crate) const NOT_CONNECTED: &str =
    "the workspace isn't connected to a repository yet — run `dev_sync init` to connect it";


// ===============
// === Context ===
// ===============

/// The environment every command runs in: the git runner, this machine's name and the global options.
pub(crate) struct Context {
    pub(crate) git: git::Git,
    pub(crate) host: domain::HostName,
    /// `--root`, when given.
    pub(crate) root: Option<PathBuf>,
}


// ====================
// === resolve_path ===
// ====================

/// `path` made absolute and free of symlinks; it must exist.
pub(crate) fn resolve_path(path: &Path) -> anyhow::Result<PathBuf> {
    path.canonicalize().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => anyhow::anyhow!("{} doesn't exist", path.display()),
        _ => anyhow::Error::new(error).context(format!("failed to resolve {}", path.display())),
    })
}


// ===============
// === Session ===
// ===============

/// A command that may change the workspace: it holds the lock for as long as it lives.
pub(crate) struct Session<'a> {
    pub(crate) context: &'a Context,
    workspace: workspace::Workspace,
    pub(crate) branch: domain::BranchName,
    _lock: state::Lock,
}

/// The base and the local changes to record, once clones the layout removed before are held back.
struct Settled {
    base: state::MachineState,
    changes: Vec<layout::Change>,
}

impl<'a> Session<'a> {
    /// Takes the lock, registers the merge driver, and requires the workspace to be on a branch.
    pub(crate) fn start(context: &'a Context, workspace: workspace::Workspace) -> anyhow::Result<Self> {
        let lock = state::Lock::acquire(&workspace.lock_file())?;
        let repository = workspace.repository();
        workspace::register_merge_driver(&context.git, repository)?;
        let branch = workspace::current_branch(&context.git, repository)?.with_context(|| {
            format!(
                "the workspace HEAD is detached; check out its branch first (`git -C {} switch main`)",
                repository.shell_word()
            )
        })?;
        tracing::debug!(root = %workspace.root().display(), %branch, "took the workspace lock");
        Ok(Self { context, workspace, branch, _lock: lock })
    }

    pub(crate) fn git(&self) -> &git::Git {
        &self.context.git
    }

    pub(crate) fn workspace(&self) -> &workspace::Workspace {
        &self.workspace
    }

    /// The dev folder: where the clones live.
    pub(crate) fn root(&self) -> &Path {
        self.workspace.root()
    }

    pub(crate) fn repository(&self) -> &workspace::Repository {
        self.workspace.repository()
    }

    pub(crate) fn require_no_merge(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !workspace::merge_in_progress(self.git(), self.repository())?,
            "a layout merge is in progress; resolve {} and run `dev_sync pull --continue`, or run `dev_sync pull \
             --abort`",
            self.repository().layout_file().display()
        );
        Ok(())
    }

    /// `repos.toml` at HEAD parses, and the working copy equals it, once what a run killed while committing it left
    /// behind is undone.
    pub(crate) fn require_clean_layout(&self, report: &mut report::Report) -> anyhow::Result<()> {
        if workspace::recover_interrupted_commit(self.git(), &self.workspace)? {
            let message = format!("restored {}, which an interrupted run left uncommitted", workspace::LAYOUT_FILE);
            report.info(report::Scope::Layout, message);
        }
        workspace::snapshot(self.git(), self.repository(), "HEAD")?;
        anyhow::ensure!(
            !workspace::layout_modified(self.git(), self.repository())?,
            "repos.toml has uncommitted edits; commit or discard them (`git -C {} checkout -- repos.toml`)",
            self.repository().shell_word()
        );
        Ok(())
    }

    /// Records local layout changes (§8.2, §8.3): scans the disk, puts back repositories parked by an interrupted
    /// move, detects what changed since the base, commits it, and saves the new base.
    pub(crate) fn record(&self, report: &mut report::Report) -> anyhow::Result<Recording> {
        let (git, root) = (self.git(), self.root());
        let fresh = !self.workspace.state_file().exists();
        let base = state::load(&self.workspace.state_file())?;
        let scanned = scan::scan(git, root)?;
        let restored = reconcile::restore_interrupted_moves(git, root, &base, &scanned.leftovers)?;
        let scanned = match restored.has(report::Severity::Done) {
            true => scan::scan(git, root)?,
            false => scanned,
        };
        report.extend(restored);
        let local = record::detect(&base, &scanned.repos)?;
        for path in &local.local_only {
            report.info(report::Scope::Layout, format!("{path} has no origin remote — it exists only on this machine"));
        }
        let snapshot = workspace::snapshot(git, self.repository(), "HEAD")?;
        let Settled { base, changes } = match fresh {
            true => self.hold_back_removed(&snapshot, base, local.changes, &scanned.repos, report)?,
            false => Settled { base, changes: local.changes },
        };
        tracing::debug!(fresh, changes = ?changes, "detected local layout changes");
        match record::apply_to_snapshot(&snapshot, &changes) {
            record::Recorded::Conflicts(conflicts) => {
                for conflict in conflicts {
                    report.attention(report::Scope::Layout, conflict.message);
                }
                Ok(Recording::Conflicted)
            }
            record::Recorded::Unchanged => {
                self.save_base(&snapshot, &base, &scanned.repos)?;
                Ok(Recording::Done)
            }
            record::Recorded::Changed { snapshot: recorded, applied } => {
                let message = record::commit_message(&self.context.host, &applied);
                workspace::commit_layout(git, &self.workspace, &recorded, &message)?;
                for change in &applied {
                    report.done(report::Scope::Layout, format!("recorded {change}"));
                }
                self.save_base(&recorded, &base, &scanned.repos)?;
                Ok(Recording::Done)
            }
        }
    }

    /// Makes the disk match HEAD's layout (§8.7, §8.8) and saves the resulting base, which it returns. `None` when
    /// the disk couldn't be looked at after the changes, which is in the report.
    pub(crate) fn reconcile(&self, report: &mut report::Report) -> anyhow::Result<Option<state::MachineState>> {
        let (git, root) = (self.git(), self.root());
        let target = workspace::snapshot(git, self.repository(), "HEAD")?;
        let base = state::load(&self.workspace.state_file())?;
        let scanned = scan::scan(git, root)?;
        let paths = target.repos().map(|repo| repo.path).collect::<Vec<_>>();
        let facts = reconcile::collect_facts(root, paths.iter())?;
        let plan = reconcile::plan(&base, &target, &scanned.repos, &facts);
        tracing::debug!(actions = ?plan.actions, conflicts = ?plan.conflicts, "planned the disk changes");
        let executed = reconcile::execute(git, root, &target, &plan, &base, &scanned.leftovers);
        report.extend(executed.report);
        if let Some(state) = &executed.state {
            state::save(&self.workspace.state_file(), state)?;
        }
        Ok(executed.state)
    }

    /// The repositories whose contents get synced: every one in the base (synced, or kept because its removal is
    /// blocked).
    pub(crate) fn checkouts(&self, base: &state::MachineState) -> Vec<content::Checkout> {
        let repos = base.repos.keys().map(|path| content::Checkout {
            scope: report::Scope::Repo(path.clone()),
            dir: path.to_fs_path(self.root()),
            label: path.to_string(),
        });
        repos.collect()
    }

    /// Without a state file every clone the layout lacks looks new — also one the layout removed while this machine
    /// kept it, blocked. A clone the layout listed before is taken for such a removal instead of being added back:
    /// it goes into the base as a blocked removal, so the next pull removes it only if nothing in it exists only here.
    fn hold_back_removed(
        &self,
        snapshot: &layout::Layout,
        base: state::MachineState,
        changes: Vec<layout::Change>,
        observed: &[scan::ObservedRepo],
        report: &mut report::Report,
    ) -> anyhow::Result<Settled> {
        let mut base = base;
        let mut kept = Vec::new();
        for change in changes {
            let removed = match &change {
                layout::Change::Add { path, url } if snapshot.get(path).is_none() => {
                    let repo = layout::LayoutRepo { path: path.clone(), url: url.clone() };
                    let seen = observed.iter().find(|seen| seen.path == *path);
                    match workspace::listed_before(self.git(), self.repository(), &repo)? {
                        true => seen.map(|seen| (repo, seen.id)),
                        false => None,
                    }
                }
                layout::Change::Add { .. }
                | layout::Change::Remove { .. }
                | layout::Change::Move { .. }
                | layout::Change::SetUrl { .. } => None,
            };
            match removed {
                Some((repo, id)) => {
                    let path = &repo.path;
                    report.attention(
                        report::Scope::Layout,
                        format!(
                            "{path} was removed from the layout before, and this machine has no record of keeping it \
                             — it is treated as a blocked removal: the next pull moves it to the Trash if nothing in \
                             it exists only here; run `dev_sync keep {}` to put it back instead",
                            shell::word(path.as_str())
                        ),
                    );
                    let status = state::KnownStatus::RemovalBlocked;
                    let known = state::KnownRepo { url: repo.url.clone(), id, status };
                    base.repos.insert(repo.path, known);
                }
                None => kept.push(change),
            }
        }
        Ok(Settled { base, changes: kept })
    }

    fn save_base(
        &self,
        snapshot: &layout::Layout,
        base: &state::MachineState,
        observed: &[scan::ObservedRepo],
    ) -> anyhow::Result<()> {
        let next = record::next_base(snapshot, base, observed, &BTreeSet::new());
        state::save(&self.workspace.state_file(), &next)
    }
}


// =================
// === Recording ===
// =================

/// How the record step ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum Recording {
    Done,
    /// Local changes clash with the layout; they are in the report and nothing was committed.
    Conflicted,
}
