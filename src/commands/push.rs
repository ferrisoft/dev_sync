//! `push` (§9.6).

use crate::commands::session;
use crate::content;
use crate::domain;
use crate::git;
use crate::report;
use crate::state;
use crate::workspace;


// ============
// === push ===
// ============

/// Records local layout changes, pushes every repo's branches that are ahead of their upstream, then pushes the layout.
pub(crate) fn push(context: &session::Context, report: &mut report::Report) -> anyhow::Result<()> {
    let workspace = workspace::Workspace::discover(&context.git, context.root.as_deref())?;
    let session = session::Session::start(context, workspace)?;
    session.require_no_merge()?;
    session.require_clean_layout(report)?;
    match session.record(report)? {
        session::Recording::Conflicted => Ok(()),
        session::Recording::Done => {
            let base = state::load(&session.workspace().state_file())?;
            report.extend(content::push(session.git(), &session.checkouts(&base))?);
            push_layout(&session, report)?;
            for path in base.blocked() {
                report.attention(
                    report::Scope::Layout,
                    format!(
                        "{path} was removed from the layout but is kept here because it holds work that exists only \
                         here — see `dev_sync status`"
                    ),
                );
            }
            Ok(())
        }
    }
}

/// The remote the workspace branch's upstream is on, when the layout can be pushed there.
fn layout_remote(upstream: &git::Upstream, branch: &domain::BranchName) -> anyhow::Result<domain::RemoteName> {
    match (&upstream.remote, upstream.remote_name()) {
        (git::UpstreamRemote::Unusable(name), _) => Err(workspace::unusable_remote(name, branch)),
        (_, Some(remote)) => Ok(remote.clone()),
        (_, None) => Err(anyhow::anyhow!(
            "the workspace branch {branch} tracks another local branch; set its upstream to origin (`git branch \
             --set-upstream-to=origin/{branch}`)"
        )),
    }
}

fn push_layout(session: &session::Session<'_>, report: &mut report::Report) -> anyhow::Result<()> {
    let (git, repository, branch) = (session.git(), session.repository(), &session.branch);
    let upstream = workspace::upstream(git, repository, branch)?;
    let remote = match &upstream {
        None => Ok(domain::RemoteName::origin()),
        Some(upstream) => layout_remote(upstream, branch),
    }?;
    anyhow::ensure!(
        workspace::has_remote(git, repository, remote.as_str())?,
        "the workspace repo has no {remote} remote; add one with `git -C {} remote add {remote} <url>`",
        repository.shell_word()
    );
    let invocation = match &upstream {
        Some(upstream) => {
            let refspec = format!("refs/heads/{branch}:{}", upstream.remote_ref);
            git.at(repository.dir()).args(["push", "--porcelain", remote.as_str(), &refspec])
        }
        None => git.at(repository.dir()).args(["push", "--porcelain", "-u", remote.as_str(), branch.as_str()]),
    };
    let outcome = invocation.remote(git::Prompts::Allowed)?;
    let stdout = match &outcome {
        git::RemoteOutcome::Succeeded(finished) => &finished.stdout,
        git::RemoteOutcome::Failed(failure) => &failure.stdout,
    };
    let refused = git::parse_push(stdout)?.into_iter().find(|pushed| pushed.flag == git::PushFlag::Rejected);
    match (outcome, refused) {
        (_, Some(pushed)) if pushed.summary.starts_with("[rejected]") => report.failure(
            report::Scope::Layout,
            format!("{remote} has layout changes you don't have — run `dev_sync pull`, then `dev_sync push`"),
        ),
        (_, Some(pushed)) => {
            report.failure(report::Scope::Layout, format!("{remote} refused the layout: {}", pushed.summary));
        }
        (git::RemoteOutcome::Succeeded(finished), None) => {
            let pushed_refs = git::parse_push(&finished.stdout)?;
            let updated = pushed_refs.iter().any(|pushed| pushed.flag != git::PushFlag::UpToDate);
            match updated {
                true => report.done(report::Scope::Layout, format!("pushed the layout to {remote}")),
                false => report.info(report::Scope::Layout, format!("the layout on {remote} is up to date")),
            }
        }
        (git::RemoteOutcome::Failed(failure), None) => {
            report.failure(report::Scope::Layout, failure.describe(&format!("pushing the layout to {remote}")));
        }
    }
    Ok(())
}
