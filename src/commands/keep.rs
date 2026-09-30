//! `keep <PATH>` (§9.7).

use std::path::Path;

use crate::commands::session;
use crate::domain;
use crate::git;
use crate::layout;
use crate::record;
use crate::report;
use crate::state;
use crate::workspace;


// ============
// === keep ===
// ============

/// Puts a repository whose removal is blocked back into the layout, with its URL from disk.
pub(crate) fn keep(context: &session::Context, path: &Path, report: &mut report::Report) -> anyhow::Result<()> {
    let workspace = workspace::Workspace::discover(&context.git, context.root.as_deref())?;
    let session = session::Session::start(context, workspace)?;
    session.require_no_merge()?;
    session.require_clean_layout(report)?;
    let (git, root) = (session.git(), session.root());
    let dir = session::resolve_path(path)?;
    let repo = domain::RepoPath::from_fs_path(root, &dir)?;
    let mut base = state::load(&session.workspace().state_file())?;
    let blocked = base.repos.get(&repo).is_some_and(|known| known.status == state::KnownStatus::RemovalBlocked);
    anyhow::ensure!(
        blocked,
        "{repo} isn't a blocked removal; `dev_sync keep` only puts back repositories whose removal was blocked (see \
         `dev_sync status`)"
    );
    let url = match git::origin(git, &dir)? {
        git::Origin::Url(url) => Ok(url),
        git::Origin::Missing => {
            Err(anyhow::anyhow!("{repo} has no origin remote, so it can't be put back in the layout"))
        }
    }?;
    let snapshot = workspace::snapshot(git, session.repository(), "HEAD")?;
    let addition = layout::Change::Add { path: repo.clone(), url: url.clone() };
    let kept = match snapshot.apply(std::slice::from_ref(&addition)) {
        layout::Applied::Ok(kept) => Ok(kept),
        layout::Applied::Rejected(rejections) => {
            let reasons = rejections.iter().map(|rejection| match &rejection.reason {
                layout::RejectionReason::Nested { with } => format!(
                    "the layout now has {with}, which would be nested with it — to keep the work in {repo}, push it, \
                     or move {repo} out of the workspace"
                ),
                reason @ (layout::RejectionReason::PathAbsent
                | layout::RejectionReason::PathPresent
                | layout::RejectionReason::UrlMismatch) => reason.to_string(),
            });
            Err(anyhow::anyhow!("can't put {repo} back in the layout: {}", reasons.collect::<Vec<_>>().join("; ")))
        }
    }?;
    let message = record::CommitMessage {
        subject: format!("{}: keep {repo}", context.host),
        ..record::commit_message(&context.host, &[addition])
    };
    workspace::commit_layout(git, session.workspace(), &kept, &message)?;
    let id = domain::FileId::of(&dir.join(".git"))?;
    base.repos.insert(repo.clone(), state::KnownRepo { url, id, status: state::KnownStatus::Synced });
    state::save(&session.workspace().state_file(), &base)?;
    report.done(report::Scope::Layout, format!("put {repo} back in the layout — run `dev_sync push` to publish it"));
    Ok(())
}
