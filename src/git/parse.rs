//! Pure parsers for git's machine-readable output.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;

use anyhow::Context as _;

use crate::domain;


// ============
// === Head ===
// ============

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Head {
    Branch(domain::BranchName),
    Detached(domain::CommitId),
    /// On a branch that has no commits yet.
    Unborn(domain::BranchName),
}


// ===================
// === WorkingTree ===
// ===================

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct WorkingTree {
    /// Staged or unstaged changes to tracked files.
    pub(crate) tracked_changes: bool,
    pub(crate) untracked: bool,
    pub(crate) unmerged: bool,
}


// ==============
// === Status ===
// ==============

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Status {
    pub(crate) head: Head,
    pub(crate) working_tree: WorkingTree,
}

/// Parses `git status --porcelain=v2 --branch -z`.
pub(crate) fn parse_status(output: &[u8]) -> anyhow::Result<Status> {
    let mut fields = output.split(|byte| *byte == 0);
    let mut oid = None;
    let mut head = None;
    let mut working_tree = WorkingTree::default();
    while let Some(field) = fields.next() {
        match field {
            [] | [b'!', b' ', ..] => {}
            [b'#', b' ', header @ ..] => {
                let header = std::str::from_utf8(header).context("git status printed a non-UTF-8 header")?;
                if let Some(value) = header.strip_prefix("branch.oid ") {
                    oid = Some(value.to_owned());
                } else if let Some(value) = header.strip_prefix("branch.head ") {
                    head = Some(value.to_owned());
                }
            }
            [b'1', b' ', ..] => working_tree.tracked_changes = true,
            [b'2', b' ', ..] => {
                working_tree.tracked_changes = true;
                fields
                    .next()
                    .filter(|original| !original.is_empty())
                    .context("git status printed a rename record without its original path")?;
            }
            [b'u', b' ', ..] => working_tree.unmerged = true,
            [b'?', b' ', ..] => working_tree.untracked = true,
            other => anyhow::bail!("unexpected git status record {:?}", String::from_utf8_lossy(other)),
        }
    }
    let head = match (oid.as_deref(), head.as_deref()) {
        (Some("(initial)"), Some(name)) => Head::Unborn(name.parse()?),
        (Some(oid), Some("(detached)")) => Head::Detached(oid.parse()?),
        (Some(oid), Some(name)) => oid.parse::<domain::CommitId>().and_then(|_| name.parse()).map(Head::Branch)?,
        _ => anyhow::bail!("git status printed no branch headers"),
    };
    Ok(Status { head, working_tree })
}


// =============
// === Track ===
// =============

/// A branch compared with its upstream, as of the last fetch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Track {
    InSync,
    Ahead(u32),
    Behind(u32),
    Diverged { ahead: u32, behind: u32 },
    /// The upstream branch was deleted on the remote.
    Gone,
}

/// Parses `%(upstream:track,nobracket)`: empty, `ahead N`, `behind N`, `ahead N, behind M` or `gone`.
pub(crate) fn parse_track(text: &str) -> anyhow::Result<Track> {
    let count = |text: &str, prefix: &str| -> anyhow::Result<u32> {
        let digits = text.strip_prefix(prefix).with_context(|| format!("unexpected tracking state {text:?}"))?;
        digits.parse().with_context(|| format!("unexpected commit count {digits:?}"))
    };
    match text {
        "" => Ok(Track::InSync),
        "gone" => Ok(Track::Gone),
        _ => match text.split_once(", ") {
            Some((ahead, behind)) => {
                Ok(Track::Diverged { ahead: count(ahead, "ahead ")?, behind: count(behind, "behind ")? })
            }
            None if text.starts_with("ahead ") => count(text, "ahead ").map(Track::Ahead),
            None => count(text, "behind ").map(Track::Behind),
        },
    }
}


// ======================
// === UpstreamRemote ===
// ======================

/// Where an upstream branch lives.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum UpstreamRemote {
    /// Another branch of this repository (the branch's remote is `.`).
    Local,
    Named(domain::RemoteName),
    /// A name git would take for an option: reported, never passed to git.
    Unusable(String),
}


// ================
// === Upstream ===
// ================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Upstream {
    /// E.g. `refs/remotes/origin/main`, or `refs/heads/x` for a local upstream.
    pub(crate) full_ref: String,
    pub(crate) remote: UpstreamRemote,
    /// The branch on the remote, e.g. `refs/heads/main`.
    pub(crate) remote_ref: String,
    pub(crate) track: Track,
}

impl Upstream {
    /// The remote to fetch and push, when the upstream is on one with a usable name.
    pub(crate) fn remote_name(&self) -> Option<&domain::RemoteName> {
        match &self.remote {
            UpstreamRemote::Named(name) if self.full_ref.starts_with("refs/remotes/") => Some(name),
            UpstreamRemote::Named(_) | UpstreamRemote::Local | UpstreamRemote::Unusable(_) => None,
        }
    }

    /// E.g. `origin/main`.
    pub(crate) fn short_name(&self) -> &str {
        self.full_ref
            .strip_prefix("refs/remotes/")
            .or_else(|| self.full_ref.strip_prefix("refs/heads/"))
            .unwrap_or(&self.full_ref)
    }
}


// ==================
// === BranchInfo ===
// ==================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BranchInfo {
    pub(crate) name: domain::BranchName,
    pub(crate) upstream: Option<Upstream>,
}

/// Parses `for-each-ref --format=%(refname)%00%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)%00`
/// `%(upstream:track,nobracket) refs/heads`: one line per branch.
pub(crate) fn parse_branches(output: &[u8]) -> anyhow::Result<Vec<BranchInfo>> {
    let text = std::str::from_utf8(output).context("git for-each-ref printed a non-UTF-8 branch name")?;
    text.lines().filter(|line| !line.is_empty()).map(parse_branch).collect()
}

fn parse_branch(line: &str) -> anyhow::Result<BranchInfo> {
    let fields = line.split('\0').collect::<Vec<_>>();
    let [refname, full_ref, remote, remote_ref, track] = fields.as_slice() else {
        anyhow::bail!("unexpected git for-each-ref line {line:?}");
    };
    let name = refname
        .strip_prefix("refs/heads/")
        .with_context(|| format!("unexpected branch ref {refname:?}"))?
        .parse()?;
    let remote = match *remote {
        "." => UpstreamRemote::Local,
        name => name.parse().map_or_else(|_| UpstreamRemote::Unusable(name.to_owned()), UpstreamRemote::Named),
    };
    let upstream = match *full_ref {
        "" => None,
        _ => Some(Upstream {
            full_ref: (*full_ref).to_owned(),
            remote,
            remote_ref: (*remote_ref).to_owned(),
            track: parse_track(track)?,
        }),
    };
    Ok(BranchInfo { name, upstream })
}


// ====================
// === WorktreeHead ===
// ====================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum WorktreeHead {
    Branch(domain::BranchName),
    Detached(domain::CommitId),
    Bare,
}


// ====================
// === WorktreeInfo ===
// ====================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorktreeInfo {
    pub(crate) path: PathBuf,
    pub(crate) head: WorktreeHead,
    pub(crate) prunable: bool,
}

/// Parses `git worktree list --porcelain -z`: NUL-terminated fields, records separated by an empty field. The first
/// record is the main worktree.
pub(crate) fn parse_worktrees(output: &[u8]) -> anyhow::Result<Vec<WorktreeInfo>> {
    let fields = output.split(|byte| *byte == 0).collect::<Vec<_>>();
    fields.split(|field| field.is_empty()).filter(|record| !record.is_empty()).map(parse_worktree).collect()
}

fn parse_worktree(fields: &[&[u8]]) -> anyhow::Result<WorktreeInfo> {
    let (first, rest) = fields.split_first().context("empty git worktree record")?;
    let path = first
        .strip_prefix(b"worktree ")
        .map(|path| PathBuf::from(OsStr::from_bytes(path)))
        .with_context(|| format!("unexpected git worktree record {:?}", String::from_utf8_lossy(first)))?;
    let commit = rest.iter().find_map(|field| field.strip_prefix(b"HEAD "));
    let head = rest.iter().find_map(|field| match *field {
        b"bare" => Some(Ok(WorktreeHead::Bare)),
        b"detached" => Some(
            commit
                .context("a detached worktree has no HEAD")
                .and_then(|commit| std::str::from_utf8(commit).context("non-UTF-8 commit id"))
                .and_then(str::parse)
                .map(WorktreeHead::Detached),
        ),
        _ => field.strip_prefix(b"branch refs/heads/").map(|name| {
            std::str::from_utf8(name).context("non-UTF-8 branch name").and_then(str::parse).map(WorktreeHead::Branch)
        }),
    });
    let head = head.with_context(|| format!("worktree {} has no branch, detached or bare line", path.display()))??;
    let prunable = rest.iter().any(|field| *field == b"prunable" || field.starts_with(b"prunable "));
    Ok(WorktreeInfo { path, head, prunable })
}


// ================
// === PushFlag ===
// ================

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PushFlag {
    FastForward,
    Forced,
    Deleted,
    New,
    Rejected,
    UpToDate,
}


// =================
// === PushedRef ===
// =================

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PushedRef {
    pub(crate) flag: PushFlag,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) summary: String,
}

/// Parses the stdout of `git push --porcelain`: `<flag>\t<from>:<to>\t<summary>` per ref. Other lines (`To <url>`,
/// `Done`, upstream notices) are skipped.
pub(crate) fn parse_push(output: &[u8]) -> anyhow::Result<Vec<PushedRef>> {
    let text = String::from_utf8_lossy(output);
    text.lines().filter_map(parse_pushed_ref).collect()
}

fn parse_pushed_ref(line: &str) -> Option<anyhow::Result<PushedRef>> {
    let mut parts = line.splitn(3, '\t');
    let flag = match parts.next()? {
        " " => PushFlag::FastForward,
        "+" => PushFlag::Forced,
        "-" => PushFlag::Deleted,
        "*" => PushFlag::New,
        "!" => PushFlag::Rejected,
        "=" => PushFlag::UpToDate,
        _ => None?,
    };
    let refs = parts.next()?;
    let summary = parts.next().unwrap_or_default().to_owned();
    Some(
        refs.split_once(':')
            .map(|(from, to)| PushedRef { flag, from: from.to_owned(), to: to.to_owned(), summary })
            .with_context(|| format!("unexpected git push line {line:?}")),
    )
}


// =============
// === Tests ===
// =============

#[cfg(test)]
mod tests {
    use crate::domain;
    use super::BranchInfo;
    use super::Head;
    use super::PushFlag;
    use super::Status;
    use super::Track;
    use super::Upstream;
    use super::UpstreamRemote;
    use super::WorkingTree;
    use super::WorktreeHead;
    use super::WorktreeInfo;
    use super::parse_branches;
    use super::parse_push;
    use super::parse_status;
    use super::parse_track;
    use super::parse_worktrees;

    const OID: &str = "2003f0ec48bfd542d0ecbd838ff43ae26da0e7af";

    fn branch(name: &str) -> anyhow::Result<domain::BranchName> {
        name.parse()
    }

    fn headers(head: &str) -> String {
        format!("# branch.oid {OID}\0# branch.head {head}\0# branch.upstream origin/main\0# branch.ab +0 -0\0")
    }

    #[test]
    fn parses_a_clean_status() -> anyhow::Result<()> {
        let status = parse_status(headers("main").as_bytes())?;
        assert_eq!(status, Status { head: Head::Branch(branch("main")?), working_tree: WorkingTree::default() });
        Ok(())
    }

    #[test]
    fn parses_changed_staged_untracked_and_unmerged_entries() -> anyhow::Result<()> {
        let changed = format!("{}1 .M N... 100644 100644 100644 {OID} {OID} b\0", headers("main"));
        assert_eq!(parse_status(changed.as_bytes())?.working_tree, WorkingTree {
            tracked_changes: true,
            ..WorkingTree::default()
        });
        let untracked = format!("{}? c\0", headers("main"));
        assert_eq!(parse_status(untracked.as_bytes())?.working_tree, WorkingTree {
            untracked: true,
            ..WorkingTree::default()
        });
        let unmerged = format!("{}u UU N... 100644 100644 100644 100644 {OID} {OID} {OID} f\0? err\0", headers("main"));
        assert_eq!(parse_status(unmerged.as_bytes())?.working_tree, WorkingTree {
            unmerged: true,
            untracked: true,
            ..WorkingTree::default()
        });
        Ok(())
    }

    #[test]
    fn consumes_the_original_path_of_a_rename_record() -> anyhow::Result<()> {
        let renamed = format!(
            "{}2 R. N... 100644 100644 100644 {OID} {OID} R100 a2\0? not-a-record\0\
             1 M. N... 100644 100644 100644 {OID} {OID} b\0",
            headers("main")
        );
        let status = parse_status(renamed.as_bytes())?;
        assert_eq!(status.working_tree, WorkingTree { tracked_changes: true, ..WorkingTree::default() });
        Ok(())
    }

    #[test]
    fn parses_initial_and_detached_heads() -> anyhow::Result<()> {
        let unborn = parse_status(b"# branch.oid (initial)\0# branch.head main\0")?;
        assert_eq!(unborn.head, Head::Unborn(branch("main")?));
        let detached = parse_status(format!("# branch.oid {OID}\0# branch.head (detached)\0").as_bytes())?;
        assert_eq!(detached.head, Head::Detached(OID.parse()?));
        Ok(())
    }

    #[test]
    fn rejects_malformed_status() {
        assert!(parse_status(b"").is_err());
        assert!(parse_status(format!("{}x what\0", headers("main")).as_bytes()).is_err());
        assert!(parse_status(format!("{}2 R. N... R100 a2\0", headers("main")).as_bytes()).is_err());
    }

    #[test]
    fn parses_track_strings() -> anyhow::Result<()> {
        assert_eq!(parse_track("")?, Track::InSync);
        assert_eq!(parse_track("ahead 3")?, Track::Ahead(3));
        assert_eq!(parse_track("behind 2")?, Track::Behind(2));
        assert_eq!(parse_track("ahead 1, behind 4")?, Track::Diverged { ahead: 1, behind: 4 });
        assert_eq!(parse_track("gone")?, Track::Gone);
        assert!(parse_track("ahead 99999999999").is_err());
        assert!(parse_track("sideways 1").is_err());
        Ok(())
    }

    #[test]
    fn parses_branches_with_and_without_upstreams() -> anyhow::Result<()> {
        let output = b"refs/heads/feature\0refs/remotes/origin/feature\0origin\0refs/heads/feature\0gone\n\
                       refs/heads/main\0refs/remotes/origin/main\0origin\0refs/heads/main\0ahead 1, behind 1\n\
                       refs/heads/noup\0\0\0\0\n\
                       refs/heads/local\0refs/heads/main\0.\0refs/heads/main\0\n\
                       refs/heads/odd\0refs/remotes/-x/odd\0-x\0refs/heads/odd\0\n";
        let branches = parse_branches(output)?;
        let upstream = |full_ref: &str, remote: UpstreamRemote, remote_ref: &str, track| {
            Some(Upstream { full_ref: full_ref.to_owned(), remote, remote_ref: remote_ref.to_owned(), track })
        };
        let origin = || -> anyhow::Result<UpstreamRemote> { Ok(UpstreamRemote::Named("origin".parse()?)) };
        assert_eq!(branches, vec![
            BranchInfo {
                name: branch("feature")?,
                upstream: upstream("refs/remotes/origin/feature", origin()?, "refs/heads/feature", Track::Gone),
            },
            BranchInfo {
                name: branch("main")?,
                upstream: upstream(
                    "refs/remotes/origin/main",
                    origin()?,
                    "refs/heads/main",
                    Track::Diverged { ahead: 1, behind: 1 },
                ),
            },
            BranchInfo { name: branch("noup")?, upstream: None },
            BranchInfo {
                name: branch("local")?,
                upstream: upstream("refs/heads/main", UpstreamRemote::Local, "refs/heads/main", Track::InSync),
            },
            BranchInfo {
                name: branch("odd")?,
                upstream: upstream(
                    "refs/remotes/-x/odd",
                    UpstreamRemote::Unusable("-x".to_owned()),
                    "refs/heads/odd",
                    Track::InSync,
                ),
            },
        ]);
        assert!(parse_branches(b"refs/heads/x\0refs/remotes/o/x\0o\0refs/heads/x\0ahead 99999999999\n").is_err());
        assert!(parse_branches(b"refs/heads/x\0only two\n").is_err());
        assert_eq!(parse_branches(b"")?, vec![]);
        Ok(())
    }

    #[test]
    fn parses_a_worktree_list() -> anyhow::Result<()> {
        let output = format!(
            "worktree /r\0HEAD {OID}\0branch refs/heads/main\0\0\
             worktree /wt1\0HEAD {OID}\0branch refs/heads/wtb\0\0\
             worktree /wt2\0HEAD {OID}\0detached\0\0\
             worktree /wt3\0HEAD {OID}\0branch refs/heads/gone\0\
             prunable gitdir file points to non-existent location\0\0"
        );
        let worktrees = parse_worktrees(output.as_bytes())?;
        assert_eq!(worktrees, vec![
            WorktreeInfo { path: "/r".into(), head: WorktreeHead::Branch(branch("main")?), prunable: false },
            WorktreeInfo { path: "/wt1".into(), head: WorktreeHead::Branch(branch("wtb")?), prunable: false },
            WorktreeInfo { path: "/wt2".into(), head: WorktreeHead::Detached(OID.parse()?), prunable: false },
            WorktreeInfo { path: "/wt3".into(), head: WorktreeHead::Branch(branch("gone")?), prunable: true },
        ]);
        let bare = parse_worktrees(b"worktree /b.git\0bare\0\0")?;
        assert_eq!(bare, vec![WorktreeInfo { path: "/b.git".into(), head: WorktreeHead::Bare, prunable: false }]);
        assert!(parse_worktrees(b"HEAD x\0\0").is_err());
        Ok(())
    }

    #[test]
    fn parses_push_porcelain_skipping_other_lines() -> anyhow::Result<()> {
        let output = b"To /x/remote.git\n \trefs/heads/main:refs/heads/main\t2003f0e..53666a1\n\
                       !\trefs/heads/dev:refs/heads/dev\t[rejected] (fetch first)\n\
                       *\trefs/heads/newb:refs/heads/newb\t[new branch]\n\
                       branch 'newb' set up to track 'origin/newb'.\n\
                       =\trefs/heads/old:refs/heads/old\t[up to date]\nDone\n";
        let refs = parse_push(output)?;
        let flags = refs.iter().map(|pushed| pushed.flag).collect::<Vec<_>>();
        assert_eq!(flags, vec![PushFlag::FastForward, PushFlag::Rejected, PushFlag::New, PushFlag::UpToDate]);
        let rejected = refs.get(1).ok_or_else(|| anyhow::anyhow!("missing ref"))?;
        assert_eq!(rejected.from, "refs/heads/dev");
        assert_eq!(rejected.to, "refs/heads/dev");
        assert_eq!(rejected.summary, "[rejected] (fetch first)");
        Ok(())
    }
}
