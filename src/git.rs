//! Everything that runs git: the process environment, network policy, typed queries and porcelain parsers.

mod failure;
mod parse;
mod repo;
mod runner;

pub(crate) use failure::RemoteFailure;
pub(crate) use failure::RemoteFailureKind;
pub(crate) use failure::RemoteOutcome;
#[cfg(test)]
pub(crate) use parse::BranchInfo;
pub(crate) use parse::Head;
pub(crate) use parse::PushFlag;
pub(crate) use parse::Track;
pub(crate) use parse::Upstream;
pub(crate) use parse::UpstreamRemote;
#[cfg(test)]
pub(crate) use parse::WorkingTree;
pub(crate) use parse::WorktreeHead;
pub(crate) use parse::WorktreeInfo;
pub(crate) use parse::parse_push;
pub(crate) use repo::History;
pub(crate) use repo::Operation;
pub(crate) use repo::Origin;
pub(crate) use repo::RepoStatus;
pub(crate) use repo::branches;
pub(crate) use repo::has_stash;
pub(crate) use repo::inspect;
pub(crate) use repo::operation;
pub(crate) use repo::origin;
pub(crate) use repo::probe_history;
pub(crate) use repo::status;
pub(crate) use repo::unpushed_from_head;
pub(crate) use repo::unpushed_on_branches;
pub(crate) use repo::worktrees;
pub(crate) use runner::Access;
pub(crate) use runner::Git;
pub(crate) use runner::NetworkPolicy;
pub(crate) use runner::Prompts;
