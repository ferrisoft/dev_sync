//! `import <DIR>` (§9.8).

use std::path::Path;

use crate::commands::session;
use crate::git;
use crate::layout;
use crate::record;
use crate::report;
use crate::scan;
use crate::workspace;


// ==============
// === import ===
// ==============

/// Adds every repository found under `dir` to the layout, with paths relative to `dir`. Clones nothing and never
/// touches `dir`; the next pull clones them. Any clash with the layout imports nothing.
pub(crate) fn import(context: &session::Context, dir: &Path, report: &mut report::Report) -> anyhow::Result<()> {
    let workspace = workspace::Workspace::discover(&context.git, context.root.as_deref())?;
    let session = session::Session::start(context, workspace)?;
    session.require_no_merge()?;
    session.require_clean_layout(report)?;
    let (git, root) = (session.git(), session.root());
    let source = session::resolve_path(dir)?;
    anyhow::ensure!(
        !source.starts_with(root) && !root.starts_with(&source),
        "{} {} the workspace; `dev_sync import` takes repositories from another tree — clones inside the workspace \
         are recorded by `dev_sync push`",
        source.display(),
        if source.starts_with(root) { "is inside" } else { "contains" }
    );
    let scanned = scan::scan(git, &source)?;
    let snapshot = workspace::snapshot(git, session.repository(), "HEAD")?;
    let mut additions = Vec::new();
    let mut problems = Vec::new();
    for repo in scanned.repos {
        match repo.origin {
            git::Origin::Missing => report.info(
                report::Scope::Layout,
                format!("{} has no origin remote — not imported", repo.path),
            ),
            git::Origin::Url(url) => match snapshot.get(&repo.path) {
                Some(entry) if entry.url == url => {}
                Some(entry) => problems.push(format!(
                    "{} is in the layout as {}, but has origin {url} there",
                    repo.path,
                    entry.url
                )),
                None => additions.push(layout::Change::Add { path: repo.path, url }),
            },
        }
    }
    let imported = match snapshot.apply(&additions) {
        layout::Applied::Ok(imported) => imported,
        layout::Applied::Rejected(rejections) => {
            problems.extend(rejections.iter().map(|rejection| match &rejection.reason {
                layout::RejectionReason::Nested { with } => {
                    format!("{} and {with} would be nested", rejection.change.first_path())
                }
                reason @ (layout::RejectionReason::PathAbsent
                | layout::RejectionReason::PathPresent
                | layout::RejectionReason::UrlMismatch) => {
                    format!("{} can't be added: {reason}", rejection.change.first_path())
                }
            }));
            snapshot.clone()
        }
    };
    anyhow::ensure!(problems.is_empty(), "nothing was imported: {}", problems.join("; "));
    match additions.len() {
        0 => report.info(report::Scope::Layout, format!("nothing new to import from {}", source.display())),
        count => {
            let repos = report::plural(u32::try_from(count).unwrap_or(u32::MAX), "repo");
            let message = record::CommitMessage {
                subject: format!("{}: import {repos} from {}", context.host, source.display()),
                ..record::commit_message(&context.host, &additions)
            };
            workspace::commit_layout(git, session.workspace(), &imported, &message)?;
            let them = if count == 1 { "it" } else { "them" };
            let message = format!("imported {repos} from {} — `dev_sync pull` clones {them}", source.display());
            report.done(report::Scope::Layout, message);
        }
    }
    Ok(())
}
