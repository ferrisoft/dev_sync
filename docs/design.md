# dev_sync — design and implementation brief

**Status:** design approved by the user on 2026-09-30 in a planning session, and implemented
the same day (Tasks 1–18); §19 lists every deviation.

**Audience:** the session that implements the tool. That session starts with no other
context, so everything it needs is written here. Read the whole file before writing code.
When this brief and reality disagree during implementation, pick the option that best
serves the principles in §4 and §11, and record the change in §19 (Deviations log).

**Location:** `~/dev/dev_sync` (this repo). Crate and binary name: `dev_sync`.

## Contents

1. What the tool is
2. Background and requirements
3. Glossary
4. Decisions and rejected alternatives
5. Architecture
6. File formats and templates
7. Domain types
8. Algorithms
9. Commands
10. Git integration
11. Error handling and safety rules
12. Code structure, dependencies, toolchain, lints
13. Testing
14. Implementation plan
15. Review with five independent agents
16. Rules for the implementing session
17. Environment facts
18. Open questions for the user
19. Deviations log

---

## 1. What the tool is

`dev_sync` keeps the user's dev folder, a tree of independent git clones, identical across
machines.

> **Revised on 2026-09-30, after the first implementation (§19, "Installed tool, hidden
> workspace repository").** dev_sync is installed once per machine and run from `PATH`; the
> `./sync` launcher, the `.setup` clone of the tool and the self-update are gone. The
> workspace repository lives in the dev folder's hidden `.dev_sync` folder, so the dev folder
> itself is not a git repository. This section describes the revised design; later sections
> still describe the original one where §19 says it changed.

- The dev folder (the **workspace**, e.g. `~/dev`) holds the clones and one hidden folder,
  `.dev_sync`: the **workspace repository**, a small git repo that tracks only `repos.toml`
  (repo path → origin URL) and `.gitattributes`. The clones are ordinary clones.
- `dev_sync pull` merges layout changes from the workspace repository's remote, then makes
  the disk match the layout: clones new repos, moves moved ones, trashes removed ones (only
  when nothing would be lost), updates changed URLs, and fast-forwards each repo's current
  branch.
- `dev_sync push` records local layout changes (detected by scanning the disk), pushes each
  repo's branches that are ahead of their upstream, then pushes the workspace repository.
- Real conflicts (e.g. the same path added with different URLs on two machines) stop the
  pull with git conflict markers in `.dev_sync/repos.toml`. The user resolves them and runs
  `dev_sync pull --continue`.
- dev_sync finds the workspace by walking up from the current directory to the first folder
  holding `.dev_sync/repos.toml`, so it runs from anywhere inside the dev folder; `--root`
  names it explicitly.

Typical use (the tool repo is `ferrisoft/dev_sync`; the workspace repo URL is an example):

```sh
# Once per machine: install dev_sync into ~/.cargo/bin (install.sh; the repo is private for now)
git clone git@github.com:ferrisoft/dev_sync.git && sh dev_sync/install.sh
# ... or, once the repo is public:
curl -fsSL https://raw.githubusercontent.com/ferrisoft/dev_sync/main/install.sh | sh

# Once, on the first machine: make the dev folder a workspace and publish it
dev_sync init ~/dev
git -C ~/dev/.dev_sync remote add origin git@github.com:wdanilo/dev.git
cd ~/dev && dev_sync push     # records the clones already there, publishes the layout

# Once, on every other machine
git clone git@github.com:wdanilo/dev.git ~/dev/.dev_sync && cd ~/dev && dev_sync pull

# Daily, from anywhere inside ~/dev
dev_sync pull              # before starting work
dev_sync push              # before switching machines
dev_sync status            # what push/pull would do, and what exists only on this machine (no network)
```

---

## 2. Background and requirements

The user keeps every development repo in one folder (today `~/dev`) as a tree of
independent clones, for example `a`, `b/c`, `b/d`, `b/e`, `c/a/g`, `c/a/f`, `c/x`.
Intermediate folders (`b/`, `c/`, `c/a/`) are plain folders; each leaf is a clone of a
GitHub repo, from several organizations (mostly `ferrisoft`, some `wdanilo`). The user
works on several NixOS machines (the laptop, `demeter`, dev VMs such as `dev-1`) and wants:

1. the structure remembered in a git repo;
2. the structure recreated on any machine with one command;
3. the machines kept in sync with git-like `./sync push` and `./sync pull`, where a
   genuine conflict stops and is left to the user to resolve.

Constraints the user stated:

- **Single user, single folder.** Only this user, only the dev folder, never anywhere else.
  Nothing needs to be generic or configurable: no config file, no multi-workspace support,
  fixed file names.
- **The workspace folder is its own git repo** (planned `~/dev2`), not part of the dotfiles
  repo.
- **Tool and data must not share a repo** (§4.2).
- **Rust.** Tests, no panics, careful error handling including network problems, and
  invariants enforced by types.
- **After implementing, five fresh max-effort agents review the code** (§15).

---

## 3. Glossary

| Term | Meaning |
|---|---|
| workspace | The dev folder (e.g. `~/dev`): the clones, plus the workspace repo in its hidden `.dev_sync` folder (revised, §19; originally the dev folder itself was the repo). |
| workspace repo | The git repo in `<workspace>/.dev_sync` that tracks only the layout files. Its remote (`origin`) is a private GitHub repo. |
| repo | An independent git clone inside the workspace. |
| layout | The mapping repo path → origin URL, stored in `repos.toml` at the workspace root. |
| snapshot | The layout as committed at some commit (HEAD, the upstream, or the merge base). |
| base | Per-machine record of what this machine had on disk after its last successful sync. Stored in `.git/dev_sync/state.toml`; never committed. |
| observed | What a scan of the disk finds right now. |
| target | The layout the disk must match after a pull: HEAD's snapshot after the merge. |
| pending | In the snapshot but not on disk (e.g. its clone failed). Every pull retries it. |
| blocked removal | Removed from the layout elsewhere, but the local clone holds work that exists only here. Kept on disk and reported until resolved. |
| local-only repo | A clone with no `origin` remote. Can't be synced; reported on every run. |
| tool repo | This repo. Installed on each machine with `cargo install` (revised, §19; originally the launcher cloned it into `<workspace>/.setup`). |
| launcher | Removed (§19). Was `<workspace>/sync`, the script the user ran. |

---

## 4. Decisions and rejected alternatives

These were settled with the user. Don't reopen them; if one proves impossible, log it in §19.

### 4.1 Why a custom tool

- **A plain manifest script** (bash: a `path url` file plus a clone loop). A tested version
  worked for recording and recreating, but it can't merge, detect moves, handle conflicts or
  protect local work.
- **git submodules.** They pin commits, so the parent repo goes dirty on every commit in any
  child, and fresh clones land on a detached HEAD. That's the wrong model for independently
  developed repos.
- **vcstool / vcs2l.** vcstool was removed from nixpkgs as "unmaintained upstream since
  January 2022"; its maintained fork `vcs2l` (1.1.7, from ros-infrastructure) is in
  nixpkgs. Tested on 2026-09-30 against a mock of the layout. It works, but has four
  problems that matter here:
  1. `vcs export` drops any repo whose current branch has no upstream. The only signs are
     one warning on stderr and exit code 1.
  2. Re-running `vcs import` on an existing tree switches repos back to the recorded branch
     unless you pass `--skip-existing`.
  3. When the workspace root is itself a git repo, plain `vcs export` lists nothing below it,
     and `--nested` prefixes every path with the root folder's name.
  4. `vcs import` refuses a target directory that doesn't exist yet.
- **ghq, mr, mani and similar.** They impose a host/org folder layout or have no conflict
  handling. Folder names here don't match repo names (`ferrisoft/setup` is
  `ferrisoft/handbook.git`), and two paths can share a URL (`account_manager` and
  `claude-status`).

### 4.2 Tool and data live in separate repos (the approved "option 2")

- **One repo for tool and data: rejected.** Their lifecycles collide. The layout is written
  by the tool, only moves forward and is synced constantly. The tool is code with branches,
  uncommitted edits and `git bisect`. In one checkout, `repos.toml` follows whatever is
  checked out: layout commits land on experiment branches, bisecting rewinds the layout
  under `./sync`, and uncommitted tool edits make `git merge` refuse whenever a pull touches
  the same files.
- **Tool installed from `/etc/nixos` (like the user's `git-home`): rejected.** Every tool
  change would need a rebuild on each host, and per the user's own note on `git-home`,
  changing what the dev VMs get restarts them.
- **Tool and data inside the dotfiles repo: rejected by the user.**
- **Chosen:** the workspace repo holds only the layout. The tool is its own repo, cloned by
  the launcher into `<workspace>/.setup` (Google's `repo` tool works the same way with
  `.repo/repo`) and fast-forwarded by `pull` (§8.11).
- **Revised on 2026-09-30 by the user (§19):** the tool is installed once per machine
  (`cargo install`) and run from `PATH`; no launcher, no `.setup`, no self-update. Tool and
  data stay in separate repos.

### 4.3 Other settled decisions

- **Layout changes are detected from the disk.** The user clones, moves (`mv`) and trashes
  repos with ordinary tools; there are no `add`/`rm`/`mv` commands.
- **Moves are identified by the inode of the `.git` directory plus the same origin URL.**
  `mv` within one filesystem keeps the inode, so detection is exact even when two clones
  share a URL.
- **Removals never delete.** They move the clone to the freedesktop Trash (the `trash`
  crate), and only if nothing in it exists solely on this machine (§8.9). Otherwise the
  removal is blocked and reported.
- **`repos.toml` merges semantically** through a git merge driver (§8.5, §9.9), so only real
  conflicts stop a pull.
- **The layout file is TOML with one repo per line**, not JSON. Conflict hunks are then whole
  lines with no commas to fix up, and TOML allows comments.
- **Repo contents are synced too** (the approved design includes it): fetch and
  fast-forward-only on pull; on push, push branches that are ahead of an existing upstream.
  Never force-push, never publish a branch that has no upstream, never merge or rebase repo
  contents for the user.
- **The tool calls the `git` command**, not libgit2 or gitoxide, so the user's ssh-agent,
  `~/.ssh/config`, credential helpers and `insteadOf` rules all keep working.
- **`status` never touches the network.**
- **Stable Rust** (§12.3).
- **The launcher lives in the workspace repo**; everything else lives in the tool repo.
  (Revised, §19: there is no launcher.)

---

## 5. Architecture

### 5.1 Workspace (example `~/dev`; revised, §19)

```
~/dev/                          not a git repository
├── .dev_sync/                  the workspace repo
│   ├── .git/
│   │   └── dev_sync/
│   │       ├── state.toml      per-machine base (never committed)
│   │       └── lock            held while a dev_sync command runs
│   ├── .gitattributes          tracked: `repos.toml merge=dev-sync`
│   └── repos.toml              tracked: the layout (§6.1)
├── account_manager/            repos
├── ferrisoft/
│   ├── design_system/
│   └── website/
└── …
```

Originally the dev folder itself was the workspace repo, with `.gitignore`, `.gitattributes`,
`repos.toml`, the `sync` launcher and the tool's `.setup` clone at its top.

Nothing hardcodes the folder. The root is wherever `.dev_sync/repos.toml` sits, so the user
can move or rename the dev folder.

### 5.2 Tool repo (this repo)

A standard cargo project, binary crate only (no lib): `Cargo.toml`, `Cargo.lock`
(committed), `CLAUDE.md`, `docs/design.md` (this file), `src/`, `templates/`, `tests/`.
While developing, the user runs it straight from `~/dev/dev_sync` with
`cargo run -- --root <workspace> …`. In use, it is installed with `cargo install` and run from
`PATH` (revised, §19; originally the launcher ran it from `<workspace>/.setup`).

### 5.3 Two kinds of state, synced separately

1. **Layout** (which repo lives at which path). Stored in the workspace repo and synced by
   that repo's push/pull, with a semantic merge for `repos.toml`.
2. **Contents** (each repo's commits). Synced through each repo's own remote. `dev_sync`
   drives this only in safe ways: fetch, fast-forward, push of already-published branches.

### 5.4 The per-machine base and the core invariant

After every successful `record` (§8.2–8.3) or `reconcile` (§8.7–8.8), the base holds exactly:

- every repo that is on disk **and** in the snapshot with the same URL (status `synced`);
- every blocked removal (on disk, not in the snapshot; status `removal-blocked`);
- every failed removal (on disk, not in the snapshot; status `synced`, retried next pull).

Pending repos (clone failed) are **not** in the base, so a repo that is missing because it
was never cloned is never mistaken for a local deletion. Comparing the base with a fresh
scan is how local changes are found; comparing the base with the target is how a pull knows
what to do on disk.

---

## 6. File formats and templates

### 6.1 `repos.toml`

Canonical form, exactly as the tool writes it:

```toml
# dev_sync workspace layout: repository path -> origin URL.
# Written by ./sync. Edit it by hand only to resolve a merge conflict.
format = 1

[repos]
"account_manager" = { url = "git@github.com:ferrisoft/account_manager.git" }
"claude-status" = { url = "git@github.com:ferrisoft/account_manager.git" }
"ferrisoft/setup" = { url = "git@github.com:ferrisoft/handbook.git" }
```

**Rendering rules** (render → parse must round-trip, and rendering must be byte-stable):

- the two comment lines, `format = 1`, one empty line, `[repos]`, then one line per repo,
  sorted by `RepoPath` order (byte order of the UTF-8 path). The file ends with exactly one
  `\n`. An empty layout ends right after the `[repos]` line.
- Each line is `<quoted path> = { url = <quoted url> }`.
- Quoting uses TOML basic strings: escape `\` as `\\` and `"` as `\"`. `RepoPath` and
  `RemoteUrl` forbid control characters (§7), so nothing else needs escaping.

**Parsing rules:**

1. Reject unresolved conflict markers before any TOML parsing: a line starting with
   `<<<<<<<`, `|||||||`, `=======` or `>>>>>>>`. The error lists every offending 1-based
   line number and says to resolve them and run `./sync pull --continue`.
2. Deserialize with serde, `deny_unknown_fields` on the top level and on entries. Accept any
   valid TOML (not just the canonical form).
3. `format` is required. `1` is accepted. Anything greater gives: "repos.toml uses format N,
   but this dev_sync only understands format 1 — update the tool (`./sync pull` updates
   it)". Any other value is invalid.
4. `[repos]` is optional; missing means an empty layout.
5. Convert each key to a `RepoPath` and each url to a `RemoteUrl`, then build a `Layout`,
   which rejects nested paths (§7). Errors name the offending key.

**Merge-driver inputs only:** empty or whitespace-only text means an empty layout. Git passes
an empty base when the file didn't exist at the merge base.

### 6.2 `state.toml` (per machine)

Location: `<git-dir>/dev_sync/state.toml`, where `<git-dir>` comes from
`git -C <root> rev-parse --absolute-git-dir` (normally `<root>/.git`).

```toml
format = 1

[repos."account_manager"]
url = "git@github.com:ferrisoft/account_manager.git"
device = 43
inode = 1234567
status = "synced"            # or "removal-blocked"
```

- A missing file means an empty base (first run on this machine).
- Write it atomically: write `state.toml.tmp` in the same directory, fsync, then rename it
  over `state.toml`.
- Any other `format` value is an error naming the file.
- Deleting this file must be harmless: the next run treats every observed repo as a local
  addition, which is a no-op for repos already in the snapshot (see the test "lost state is
  harmless", §13.2).

### 6.3 Lock file

`<git-dir>/dev_sync/lock` is created with `create_new` (fails if it exists) and holds the
PID. If it already exists: when `/proc/<pid>` exists, fail with "another dev_sync is running
(pid N)"; otherwise the lock is stale, so replace it. Remove it when the command ends,
including on error paths (a guard type whose `Drop` removes it). `merge-driver` never takes
the lock: it runs inside a locked `pull`, or inside a merge the user started by hand.

### 6.4 Templates

**Revised (§19):** only `templates/gitattributes` remains, written into `.dev_sync/`. The
workspace repo's work tree holds nothing but its own files, so it needs no `.gitignore`, and
there is no launcher.

Keep these in `templates/` and embed them with `include_str!`. Exact contents:

`templates/gitignore` → written as `<workspace>/.gitignore`:

```
# dev_sync workspace. Only the workspace's own files are tracked here;
# the repositories inside are independent clones listed in repos.toml.
/*
!/.gitignore
!/.gitattributes
!/repos.toml
!/sync
```

`templates/gitattributes` → `<workspace>/.gitattributes`:

```
repos.toml merge=dev-sync
```

`templates/sync` → `<workspace>/sync`, mode 0755. `__TOOL_URL__` is replaced by the tool URL
quoted for sh in single quotes, with each `'` written as `'\''`:

```bash
#!/usr/bin/env bash
# dev_sync launcher: runs the tool from .setup, cloning it first if it is missing.
# It must not change the working directory: git runs the merge driver through this
# script and passes file names relative to the current directory.
set -euo pipefail
root=$(cd -- "$(dirname -- "$(realpath -- "$0")")" && pwd)
tool="$root/.setup"
if [ ! -d "$tool/.git" ]; then
    git clone --quiet -- __TOOL_URL__ "$tool"
fi
if ! command -v cargo >/dev/null 2>&1; then
    echo "sync: cargo not found; dev_sync needs a Rust toolchain" >&2
    exit 1
fi
export CARGO_TARGET_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/dev_sync/target"
exec cargo run --quiet --manifest-path "$tool/Cargo.toml" -- --root "$root" "$@"
```

The build output lives in `~/.cache/dev_sync/target`, so nothing generated sits in the dev
folder.

---

## 7. Domain types

Invariants belong in types: construct domain values only through validating constructors
(private fields), and model expected outcomes as enums. The names below are the intended
interface; keep the semantics even if a name changes. Visibility is `pub(crate)` throughout
(binary crate). No tuples in signatures; use small named structs.

**`RepoPath`** (`domain/repo_path.rs`): a repo's location relative to the workspace root,
e.g. `ferrisoft/website`.

- A UTF-8 string of `/`-separated components with at least one component. Every component is
  non-empty, not `.` or `..`, doesn't start with `.` (hidden names are reserved for tooling:
  `.git`, `.setup`, temp dirs), and contains no control character (`char::is_control`). No
  leading or trailing `/`, no `//`. Invalid input is rejected, never normalized. Spaces,
  quotes, backslashes and non-ASCII are allowed.
- `impl FromStr` (`Err = anyhow::Error`), `as_str`, `Display`, `Ord` (byte order of the
  string).
- `components() -> impl Iterator<Item = &str>`
- `to_fs_path(&self, root: &Path) -> PathBuf`
- `from_fs_path(root: &Path, path: &Path) -> anyhow::Result<Self>`: strips the root; a
  non-UTF-8 path is an error.
- `overlaps(&self, other: &Self) -> bool`: true when the paths are equal or one is an
  ancestor of the other. `a` and `a/b` overlap; `a` and `ab` don't; `a-b` and `a/b` don't.
- `proper_ancestors(&self) -> Vec<Self>`: `a/b/c` → `[a, a/b]`.

**`RemoteUrl`** (`domain/remote_url.rs`): a git remote URL exactly as configured, e.g.
`git@github.com:ferrisoft/shell.git`, `https://…`, or a local path. Non-empty, no leading or
trailing whitespace, no control characters, and it must not start with `-` (defense against
option injection; git calls also put `--` before it). `FromStr`, `as_str`, `Display`, `Ord`.

**`FileId { device: u64, inode: u64 }`** (`domain/file_id.rs`): `FileId::of(path)` reads
`symlink_metadata` through `std::os::unix::fs::MetadataExt`. Identifies a `.git` directory
across `mv`.

**`HostName`** (`domain/host_name.rs`): `HostName::detect()` tries the `DEV_SYNC_HOST` env var
(tests use it), then `/proc/sys/kernel/hostname` (trimmed), then `HOSTNAME`, then falls back
to `unknown-host`. Used in commit messages.

**`BranchName`** and **`CommitId`**: validated newtypes for git branch names (non-empty, no
whitespace or control characters) and commit hashes (40 or 64 hex characters).

**Layout** (`layout/model.rs`):

```rust
pub(crate) struct Entry { pub(crate) url: RemoteUrl }
pub(crate) struct LayoutRepo { pub(crate) path: RepoPath, pub(crate) url: RemoteUrl }

/// The repositories that make up the workspace, keyed by location. No location is nested inside another.
pub(crate) struct Layout { repos: BTreeMap<RepoPath, Entry> }

impl Layout {
    pub(crate) fn from_repos(repos: Vec<LayoutRepo>) -> anyhow::Result<Self>; // rejects duplicates and nesting
    pub(crate) fn get(&self, path: &RepoPath) -> Option<&Entry>;
    pub(crate) fn repos(&self) -> impl Iterator<Item = LayoutRepo> + '_;    // sorted by path (clones are cheap)
    pub(crate) fn len(&self) -> usize;
    pub(crate) fn is_empty(&self) -> bool;
    pub(crate) fn apply(&self, changes: &[Change]) -> Applied;               // batch semantics, below
}

pub(crate) enum Applied { Ok(Layout), Rejected(Vec<ChangeRejection>) }
pub(crate) struct ChangeRejection { pub(crate) change: Change, pub(crate) reason: RejectionReason }
pub(crate) enum RejectionReason { PathAbsent, PathPresent, UrlMismatch, Nested { with: RepoPath } }
```

`Layout::apply` batch semantics, so swaps and chains work:

1. Removal phase: take out every `Remove` path and every `Move` source. Precondition: the path
   is present with the change's URL.
2. Insertion phase: insert every `Add` path and `Move` destination (precondition: absent after
   the removal phase), and apply every `SetUrl` (precondition: present, with URL `from`).
3. Check the no-nesting invariant on the result.

Any failed precondition makes the result `Rejected` with every problem listed. It never
panics and never partially applies.

**`Change`** (`layout/change.rs`):

```rust
pub(crate) enum Change {
    Add { path: RepoPath, url: RemoteUrl },
    Remove { path: RepoPath, url: RemoteUrl },
    Move { from: RepoPath, to: RepoPath, url: RemoteUrl },
    SetUrl { path: RepoPath, from: RemoteUrl, to: RemoteUrl },
}
impl Change { pub(crate) fn paths(&self) -> Vec<&RepoPath>; } // 1 path, or 2 for Move
impl Display for Change // `+path`, `-path`, `from → to`, `path: <old> → <new>`
pub(crate) fn diff(old: &Layout, new: &Layout) -> Vec<Change>; // §8.4
```

**Merge** (`layout/merge.rs`):

```rust
#[must_use]
pub(crate) enum MergeOutcome { Clean(Layout), Conflicted(ConflictedMerge) }
pub(crate) struct ConflictedMerge { pub(crate) resolved: Layout, pub(crate) conflicts: Vec<Conflict> }
pub(crate) struct Conflict {
    pub(crate) local: Vec<Change>,
    pub(crate) incoming: Vec<Change>,
    pub(crate) local_repos: Vec<LayoutRepo>,    // what `local` has at the touched paths
    pub(crate) incoming_repos: Vec<LayoutRepo>, // what `incoming` has at the touched paths
}
impl Display for Conflict // one-line description, §8.5
pub(crate) fn merge(base: &Layout, local: &Layout, incoming: &Layout) -> MergeOutcome;
```

**Disk and state:**

```rust
pub(crate) enum Origin { Url(RemoteUrl), Missing }
pub(crate) struct ObservedRepo { pub(crate) path: RepoPath, pub(crate) id: FileId, pub(crate) origin: Origin }

pub(crate) enum KnownStatus { Synced, RemovalBlocked }
pub(crate) struct KnownRepo { pub(crate) url: RemoteUrl, pub(crate) id: FileId, pub(crate) status: KnownStatus }
pub(crate) struct MachineState { pub(crate) repos: BTreeMap<RepoPath, KnownRepo> }
```

**Git-facing types** (`git/…`):

```rust
pub(crate) enum Head { Branch(BranchName), Detached(CommitId), Unborn(BranchName) }
pub(crate) enum Track { InSync, Ahead(u32), Behind(u32), Diverged { ahead: u32, behind: u32 }, Gone }
pub(crate) struct Upstream { pub(crate) full_ref: String, pub(crate) remote: String, pub(crate) remote_ref: String, pub(crate) track: Track }
pub(crate) struct BranchInfo { pub(crate) name: BranchName, pub(crate) upstream: Option<Upstream> }
pub(crate) enum Operation { Merge, Rebase, CherryPick, Revert, Bisect }
pub(crate) struct WorkingTree { pub(crate) tracked_changes: bool, pub(crate) untracked: bool, pub(crate) unmerged: bool }
pub(crate) struct RepoStatus {
    pub(crate) head: Head,
    pub(crate) working_tree: WorkingTree,
    pub(crate) operation: Option<Operation>,
    pub(crate) has_stash: bool,
    pub(crate) branches: Vec<BranchInfo>,
}
pub(crate) struct WorktreeInfo { pub(crate) path: PathBuf, pub(crate) head: WorktreeHead, pub(crate) prunable: bool }
pub(crate) enum WorktreeHead { Branch(BranchName), Detached(CommitId), Bare }

pub(crate) enum RemoteFailureKind { Network, Timeout, Auth, NotFound, Rejected, Other }
pub(crate) struct RemoteFailure { pub(crate) kind: RemoteFailureKind, pub(crate) attempts: u32, pub(crate) detail: String }
#[must_use]
pub(crate) enum RemoteOutcome { Succeeded(Finished), Failed(RemoteFailure) }

pub(crate) enum RemovalSafety { Safe, Unsafe(Vec<UnsafeReason>) }
pub(crate) enum UnsafeReason {
    UncommittedChanges,
    UntrackedFiles,
    OperationInProgress(Operation),
    Stash,
    UnpushedCommits { count: u32 },
    Worktree { path: PathBuf, problem: WorktreeProblem },
}
pub(crate) enum WorktreeProblem { UncommittedChanges, UntrackedFiles, UnpushedCommits { count: u32 } }
```

**Report** (`report.rs`):

```rust
pub(crate) enum Severity { Done, Info, Attention, Failure }
pub(crate) enum Scope { Workspace, Layout, Disk, Repo(RepoPath), Tool }
pub(crate) struct Item { pub(crate) severity: Severity, pub(crate) scope: Scope, pub(crate) message: String }
#[must_use]
pub(crate) struct Report { items: Vec<Item> }
impl Report { /* push, extend, render() -> String, exit_code() -> std::process::ExitCode */ }
```

**Other result types** (used in §8):

```rust
pub(crate) struct LocalChanges { pub(crate) changes: Vec<Change>, pub(crate) local_only: Vec<RepoPath> }
pub(crate) struct LocalConflict { pub(crate) path: RepoPath, pub(crate) message: String }
pub(crate) enum Recorded { Unchanged, Changed { snapshot: Layout, applied: Vec<Change> }, Conflicts(Vec<LocalConflict>) }

pub(crate) enum DestinationFact { Free, EmptyDir, Occupied, InsideRepo { repo: RepoPath } }
pub(crate) struct DiskFacts { pub(crate) destinations: BTreeMap<RepoPath, DestinationFact> }
pub(crate) enum DiskConflict { Occupied { path: RepoPath, detail: String }, Nested { path: RepoPath, inside: RepoPath } }
pub(crate) struct Executed { pub(crate) report: Report, pub(crate) state: MachineState }

pub(crate) enum PullDecision { FastForward { behind: u32 }, BehindDirty { behind: u32 }, Diverged { ahead: u32, behind: u32 }, UpstreamGone, Nothing }
pub(crate) struct RemotePush { pub(crate) remote: String, pub(crate) refspecs: Vec<String> }
pub(crate) struct PushPlan { pub(crate) pushes: Vec<RemotePush>, pub(crate) notes: Vec<Item> }
pub(crate) enum SelfUpdateDecision { Skip { reason: String }, FastForward { behind: u32 }, UpToDate }
```

---

## 8. Algorithms

### 8.1 Scan

`scan(git, root) -> anyhow::Result<Vec<ObservedRepo>>`, sorted by path.

- Walk recursively from the root, never following symlinks (use `DirEntry::file_type` /
  `symlink_metadata`).
- Skip every entry whose name starts with `.`, at every level. That covers `.git`, `.setup`,
  `.claude`, the tool's temp dirs, and any hidden scratch folder. **A clone under a hidden
  folder is never synced; that's the user's way to keep a scratch clone out of the layout.**
- Skip everything that isn't a directory.
- The root is the workspace repo itself; don't record it, just walk its children.
- For every other directory `D`:
  - `D/.git` is a directory → `D` is a repo. Record `FileId::of(D/.git)` and its origin, and
    **don't descend** into it.
  - `D/.git` is a file (a linked worktree or submodule checkout) or a symlink → skip `D`
    entirely: don't record it, don't descend.
  - otherwise → descend.
- Origin: `git -C D config --get remote.origin.url`. Exit 0 → `RemoteUrl::parse(trimmed)`;
  an invalid URL is a scan error naming `D`. Exit 1 → `Origin::Missing`. Any other exit is an
  error.
- **Any I/O error (unreadable directory, permission denied) or non-UTF-8 name aborts the scan
  with an error naming the path.** Skipping it silently would make the repos under it look
  removed, and that removal would propagate to every machine.
- Cost, measured on `~/dev`: stopping at repo boundaries visited 18 directories in 0.3 ms;
  walking into every working tree touched 307k entries in 1.6 s. So never descend into repos.

### 8.2 Detect local changes

`detect(base: &MachineState, observed: &[ObservedRepo]) -> anyhow::Result<LocalChanges>`,
where `LocalChanges { changes: Vec<Change>, local_only: Vec<RepoPath> }`.

1. Index the base by `FileId` and by path.
2. For each observed repo `O` with `Origin::Url(u)`:
   - Some base entry `B` has `B.id == O.id`:
     - same path: `u == B.url` → no change; otherwise `SetUrl { path, from: B.url, to: u }`.
     - different path: `u == B.url` → `Move { from: B.path, to: O.path, url: u }`; otherwise
       `Remove { B.path, B.url }` plus `Add { O.path, u }`. (An inode can be reused after a
       delete, so a move also requires the same URL.)
     - Mark `B` matched.
   - Otherwise, an unmatched base entry `B` exists at `O.path` (the clone was replaced, e.g.
     deleted and cloned again): `u == B.url` → no change, else `SetUrl`. Mark `B` matched.
   - Otherwise → `Add { O.path, u }`.
3. For each observed repo `O` with `Origin::Missing`:
   - It matches a base entry (by id or path) → **error**: "`<path>` had origin `<url>`, but
     its origin remote is gone. Restore it (`git -C <path> remote add origin <url>`) or move
     the repo out of the workspace." Treating it as removed would trash it on every other
     machine.
   - Otherwise → add to `local_only`.
4. Every unmatched base entry `B` → `Remove { B.path, B.url }`. This includes
   removal-blocked entries the user has since deleted.

### 8.3 Apply local changes to the snapshot

`apply_to_snapshot(snapshot: &Layout, changes: &[Change]) -> Recorded`:

```rust
pub(crate) enum Recorded { Unchanged, Changed { snapshot: Layout, applied: Vec<Change> }, Conflicts(Vec<LocalConflict>) }
```

Rewrite each local change against the snapshot, then apply the rewritten set as one batch
with `Layout::apply`:

| Local change | Snapshot state | Result |
|---|---|---|
| `Add(p, u)` | `p` with `u` | nothing (a pending repo just got cloned by hand) |
| `Add(p, u)` | `p` with another URL `v` | local conflict: "`p` is in the layout as `v`, but the clone on disk has origin `u`" |
| `Add(p, u)` | absent | `Add(p, u)` (nesting with another entry → local conflict) |
| `Remove(p, u)` | present | `Remove` |
| `Remove(p, u)` | absent | nothing |
| `Move(a→b, u)` | `a` present, `b` absent or also leaving | `Move` |
| `Move(a→b, u)` | `a` absent, `b` absent | `Add(b, u)` |
| `Move(a→b, u)` | `b` present with `u` | `Remove(a)` if `a` is present, otherwise nothing |
| `Move(a→b, u)` | `b` present with another URL | local conflict |
| `SetUrl(p, _, u)` | present | `SetUrl` |
| `SetUrl(p, _, u)` | absent | nothing |

If the batch comes back `Rejected`, turn every rejection into a local conflict. **Local
conflicts abort the command before any commit, fetch or disk change.** Report each one with
a hint for fixing it on disk, and exit 2.

When the result is `Changed`: write `repos.toml` (canonical), then run
`git commit --quiet -m <message> -- repos.toml`, which commits only that file.

- First line of the message: `<host>: <summary>`. The summary is the changes joined by `, `
  (`+path`, `-path`, `a → b`, `path: new url`), cut to about 72 characters with
  `(+N more)`.
- The body lists every change with its URLs.
- Never add attribution lines.

### 8.4 Diff with move pairing

`diff(old, new) -> Vec<Change>`:

1. Paths in both layouts with different URLs → `SetUrl`.
2. `removed` = paths only in `old`; `added` = paths only in `new`.
3. Group both by URL. For each URL, zip the sorted removed paths with the sorted added paths;
   each pair becomes a `Move`, and what's left becomes `Remove` or `Add`.
4. Sort the output by each change's first path, so it's deterministic.

When several clones share a URL the pairing is arbitrary but deterministic; that's fine,
because such clones are interchangeable at the layout level.

### 8.5 Three-way merge

`merge(base, local, incoming) -> MergeOutcome`:

1. `L = diff(base, local)` and `I = diff(base, incoming)`.
2. Every change that appears identically in both is **common**: keep one copy and take it out
   of both sides.
3. A change `x ∈ L` conflicts with `y ∈ I` if any path of `x` overlaps any path of `y`
   (`RepoPath::overlaps`). Build conflict groups as connected components of this relation
   (union-find). Changes outside every group are **clean**.
4. `base.apply(common ∪ clean L ∪ clean I)`. If that is `Rejected` (it shouldn't be), put the
   rejected changes into one more conflict group. Never panic.
5. No groups → `Clean(result)`. Otherwise → `Conflicted`:
   - `resolved` = the result minus every path touched by any group (base entries at those
     paths too);
   - each `Conflict` holds its local and incoming changes, plus the entries `local` and
     `incoming` have at the group's touched paths.
6. Description (`Display`), for example:
   - `"shell": moved to "ferrisoft/shell" locally, to "apps/shell" in the incoming change`
   - `"tools": added locally as git@github.com:a/tools.git, incoming as git@github.com:b/tools.git`
   - generic: `local: <changes>; incoming: <changes>`

The merge driver (§9.9) and `pull` (§9.5, for the report) call this same function, so both
always agree.

### 8.6 Conflict rendering and resolution

`render_conflicted(&ConflictedMerge) -> String` writes the canonical header and every
`resolved` entry, then one block per conflict (ordered by first touched path):

```
# CONFLICT: "tools": added locally as git@github.com:a/tools.git, incoming as git@github.com:b/tools.git
# Keep the lines you want, delete the rest and the marker lines, then run ./sync pull --continue
<<<<<<< local
"tools" = { url = "git@github.com:a/tools.git" }
=======
"tools" = { url = "git@github.com:b/tools.git" }
>>>>>>> incoming
```

- A side with no entries (e.g. the removing side of move-vs-remove) renders as nothing
  between its markers.
- Parsing refuses the file while markers remain (§6.1).
- After the user resolves, `pull --continue` parses and validates the file, writes it back in
  canonical form (dropping comments and normalizing formatting), stages it and finishes the
  merge commit.

### 8.7 Reconcile, part 1: plan (pure, unit-tested)

`plan(base: &MachineState, target: &Layout, observed: &[ObservedRepo], facts: &DiskFacts) -> Plan`.

`DiskFacts` is collected before planning, so the planner stays pure. For every candidate
destination path it records whether a non-repo file or directory exists there (and whether
it's an empty directory), and which proper ancestors of it are repos on disk.

```rust
pub(crate) enum Action {
    Adopt { path: RepoPath },                      // already on disk with the right URL; just record it
    Remove { path: RepoPath },                     // safety check at execution time
    Move { from: RepoPath, to: RepoPath },
    SetUrl { path: RepoPath, url: RemoteUrl },
    Clone { path: RepoPath, url: RemoteUrl },
}
pub(crate) struct Plan { pub(crate) actions: Vec<Action>, pub(crate) conflicts: Vec<DiskConflict> }
```

Steps:

1. `leaving` = base entries whose path is missing from the target, or whose target URL
   differs. `arriving` = target entries whose path is missing from the base, or whose base
   URL differs.
2. An arriving entry already observed at its path with the same URL → `Adopt`, and drop it
   from `arriving`. The same path with a different URL, where the observed repo isn't leaving
   → `DiskConflict::Occupied` ("`p` is a clone of `<other url>`").
3. Pair `leaving` with `arriving` by URL (sorted zip per URL) → `Move { from, to }`. This
   covers plain moves, chains (`a→b`, `b→c`) and swaps (`a↔b`); execution moves in two
   phases.
4. Leftover leaving and arriving entries at the **same path** → `SetUrl { path, url }`. The
   remote changed the origin, e.g. after a repo transfer.
5. Leftover `leaving` → `Remove`. Leftover `arriving` → `Clone`.
6. Check every `Move`/`Clone` destination `q`:
   - a non-repo, non-empty file or directory at `q` → `DiskConflict::Occupied` (an empty
     directory is fine and gets removed);
   - a proper ancestor of `q` is a repo on disk that isn't leaving → `DiskConflict::Nested`
     ("`q` would be inside repo `<x>`").

   A destination that equals a leaving path is allowed, because the leaving repo moves or is
   removed first. If that removal turns out to be blocked, execution reports the occupied
   destination.

### 8.8 Reconcile, part 2: execute

`execute(git, root, plan, base) -> anyhow::Result<Executed>`, where
`Executed { report, state }`. Order:

1. **Clean up** any `.dev_sync-cloning-*` directories left by a crashed run. They're always
   ours; match that exact prefix only. Report any `.dev_sync-moving-*` directories and
   **never delete them**, since they hold real repos.
2. **Removals.** For each `Remove(p)`, run the safety check (§8.9):
   - `Safe` → `trash::delete(p)`. On success, drop `p` from the base and prune empty ancestor
     directories. On failure, report it and keep the base entry, so the next pull retries.
   - `Unsafe(reasons)` → keep the clone and set its base status to `RemovalBlocked`. Report
     attention with the reasons and the fix: "push or discard the work, then `./sync pull`
     again; or `./sync keep <p>` to put it back in the layout".

   Blocked entries are in the base and absent from the target, so every pull re-plans them
   as `Remove` and retries automatically.
3. **Moves, in two phases.**
   - Phase 1: rename every source to `<root>/.dev_sync-moving-<n>` (same filesystem as the
     root).
   - Phase 2: rename every temp to its destination, creating parent directories. If the
     destination is occupied, or the rename fails, try to rename the temp back to its source.
     If that also fails, report a failure naming the temp path, and never delete it.
   - Afterwards, if the repo has linked worktrees (`<dest>/.git/worktrees/` exists), repair
     them. Each `.git/worktrees/<name>/gitdir` file holds the absolute path of a worktree's
     `.git` file. If that path was under the old repo path, rewrite the prefix to the new one
     and collect the worktree directory (the parent of that `.git` file). Then run
     `git -C <dest> worktree repair <collected dirs…>`. With no arguments, repair still fixes
     the links from worktrees that live outside the repo.
   - Prune empty ancestor directories of every source.
   - A cross-device rename (EXDEV) is reported as a failure; never copy instead.
4. **URL changes:** `git -C <p> remote set-url origin <url>`.
5. **Clones**, at most 8 in parallel (`std::thread::scope`):
   - create the parent directories, then
     `git clone --quiet -- <url> <parent>/.dev_sync-cloning-<name>-<pid>`, then rename it to
     the destination;
   - on any failure, remove the temp directory (it's ours) and report the failure. The repo
     stays pending and is retried next pull;
   - if the destination became occupied meanwhile, remove the temp and report it.
6. **Update the base** (§8.12) and save the state file.

Prune empty directories by walking a removed or moved path's ancestors from nearest to the
root (excluding the root). `remove_dir` each one and stop at the first that fails (it isn't
empty). Never touch hidden directories or the root.

### 8.9 Removal safety

`removal_safety(git, repo) -> anyhow::Result<RemovalSafety>` collects **every** reason; it
doesn't stop at the first one:

- `git status --porcelain=v2 -z --untracked-files=normal`: any changed or unmerged record →
  `UncommittedChanges`; any `?` record → `UntrackedFiles`. Ignored files don't count, and the
  Trash keeps them anyway.
- An operation in progress (§10.6) → `OperationInProgress`.
- `git rev-parse --verify --quiet refs/stash` succeeds → `Stash`.
- `git rev-list --count --branches --not --remotes` > 0 → `UnpushedCommits` (commits on
  local branches that no remote-tracking ref reaches). Stale remote-tracking refs only make
  this more cautious, never less.
- Detached HEAD: `git rev-list --count HEAD --not --remotes` > 0 → `UnpushedCommits`. Skip
  this for an unborn HEAD.
- Linked worktrees (`git worktree list --porcelain`, excluding the first record, which is the
  main worktree, and records marked `prunable`): each must have a clean status (as above),
  and a detached one must have no unpushed commits. Otherwise →
  `Worktree { path, problem }`.

### 8.10 Content sync

Repos considered: every target repo present on disk, plus blocked repos. On `push`, also the
tool repo `<root>/.setup` when it's a git repo (revised, §19: there is no `.setup`).

**Fetch** (pull and push), at most 8 in parallel: `git fetch --all --prune --quiet` under the
network policy (§10.3). A failure is reported with its kind and a hint; carry on with the
other repos. A repo whose fetch failed gets no pull or push decision this run.

**Inspect** each repo into a `RepoStatus` (§10.5), then decide with pure functions:

`pull_decision(&RepoStatus) -> PullDecision`, for the current branch only:

| Condition | Decision |
|---|---|
| operation in progress | nothing (reported in status) |
| upstream `Behind(n)`, no tracked changes, no unmerged entries | `FastForward`: `git merge --ff-only --quiet @{upstream}`; report "fast-forwarded n commits" |
| upstream `Behind(n)` with tracked changes | attention: "behind by n but has uncommitted changes — commit or stash, then pull" |
| `Diverged { a, b }` | attention: "`<branch>` diverged from `<upstream>` (a ahead, b behind) — resolve with git in `<path>`" |
| `Gone` | attention: "upstream `<upstream>` no longer exists" |
| in sync, ahead, detached, unborn, or no upstream | nothing |

`push_plan(&RepoStatus) -> PushPlan`, over **all** local branches:

| Branch state | Action |
|---|---|
| upstream `Ahead(n)` | push it |
| `Diverged` | attention (not pushed) |
| `Gone` | attention "upstream gone; not pushing" |
| `Behind` or in sync | nothing |
| no upstream | info "`<branch>` has no upstream; not pushed (publish it with `git push -u`)" |

- Push in one command per remote:
  `git push --porcelain <remote> refs/heads/<b>:<remote_ref> …`. Parse the porcelain output
  per ref: `!` → rejected (attention "rejected — the remote has new commits; pull first");
  ` `, `*`, `=` → fine. **Never `--force`, never a `+` refspec.**
- On push, also report per repo, as info: uncommitted changes, untracked files, stash, and a
  detached HEAD with unpushed commits, i.e. work that won't reach the other machines.
  `status` shows the same details.

### 8.11 Self-update (plain `pull` only)

**Removed (§19):** dev_sync is installed per machine and updated by reinstalling it.

`self_update_decision(...) -> SelfUpdateDecision` is a pure function. Execution:

1. Skip when `DEV_SYNC_SELF_UPDATED=1` (this process is the re-exec), or when
   `<root>/.setup/.git` or `<root>/sync` doesn't exist (development mode).
2. Inspect `.setup`. If it has tracked changes or an operation in progress, or isn't on a
   branch with an upstream → skip with info "using the local working copy of .setup".
3. `git -C .setup fetch --quiet` under the network policy. On failure → info "couldn't check
   for tool updates (<kind>)" and continue with the current version.
4. `Behind(n)` and clean → `git merge --ff-only --quiet @{upstream}`, then replace the process
   (`std::os::unix::process::CommandExt::exec`) with `<root>/sync pull`, setting
   `DEV_SYNC_SELF_UPDATED=1`. The launcher rebuilds. If `exec` returns (it failed) → report a
   failure. Re-exec with exactly `["pull"]`; the launcher adds `--root` itself.
5. `Ahead` or `Diverged` → skip with info.

### 8.12 Updating the base

`next_base(...)`, after a record or a reconcile:

- every repo on disk that is in the target (or snapshot) with a matching URL →
  `KnownRepo { url, id: fresh FileId, status: Synced }`;
- every blocked removal → `RemovalBlocked`;
- every failed removal (still on disk, not in the target) → kept as it was;
- nothing else (pending clones and local-only repos stay out).

---

## 9. Commands

### 9.1 CLI (clap derive)

```
dev_sync [--root <DIR>] [--verbose] <COMMAND>

Commands:
  init <DIR>                    Make DIR a workspace (revised, §19: no --tool-url)
  status                        Show what push and pull would do (no network)
  pull [--continue | --abort]   Merge layout changes from the remote, apply them, update repos
  push                          Record local layout changes, push repos, push the layout
  keep <PATH>                   Put a blocked removal back into the layout
  import <DIR>                  Add the repos found under DIR to the layout (clones nothing)
  list [DIR]                    Show the folders as a tree down to the repos (added, §19)
  merge-driver <BASE> <LOCAL> <INCOMING> <PATH>   (hidden) git merge driver for repos.toml
```

**Root discovery** (revised, §19: every `repos.toml` and `.git` below means the ones in
`<root>/.dev_sync`). Use `--root` if given; it must contain `repos.toml` and `.git`.
Otherwise walk up from the current directory to the first directory containing both.
Failing that: "not inside a dev_sync workspace (no repos.toml found above `<cwd>`); pass
`--root`". `init` and `merge-driver` need no root.

### 9.2 Common preamble

For `status`, `pull`, `push`, `keep` and `import`:

1. Discover the root; take the lock (§6.3), except for `status`, which only reads.
2. Register the merge driver in the workspace repo's local config, idempotently (write only
   when the value differs):
   - `merge.dev-sync.name` = `dev_sync layout merge`
   - `merge.dev-sync.driver` = `<cmd> merge-driver %O %A %B %P`. `<cmd>` is the sh-quoted
     `<root>/sync` when `<root>/.setup/Cargo.toml` exists (the launcher can run the tool);
     otherwise the sh-quoted current executable followed by ` --root ` and the sh-quoted
     root. So tests and development runs never go through a launcher that has no `.setup`.
     `status` skips this step. (Revised, §19: `<cmd>` is always the sh-quoted running
     executable, without `--root`, which the driver never needed.)
3. The workspace HEAD must be a branch; a detached HEAD is an error.
4. No merge may be in progress (`git rev-parse --git-path MERGE_HEAD` exists), except for
   `pull --continue` and `pull --abort`. The error says to resolve `repos.toml` and run
   `./sync pull --continue`, or run `./sync pull --abort`. `status` reports it instead of
   failing.
5. `repos.toml` at HEAD must parse, and the working copy must equal HEAD. Otherwise: error
   "repos.toml has uncommitted edits; commit or discard them
   (`git -C <root> checkout -- repos.toml`)". `status` reports it instead.

### 9.3 `init <DIR> --tool-url <URL>`

**Revised (§19):** `init <DIR>` refuses a `DIR/.dev_sync` that exists and a `DIR` inside another
workspace, creates the workspace repo in `DIR/.dev_sync` with `.gitattributes` and an empty
`repos.toml`, registers the merge driver and commits. Clones already in `DIR` stay as they are.
The original steps:

1. Refuse if `DIR/.git` or `DIR/repos.toml` already exists.
2. Create `DIR` if needed, then `git init --quiet --initial-branch=main DIR`.
3. Write `.gitignore`, `.gitattributes`, `repos.toml` (empty, canonical) and `sync` (mode
   0755) from the templates.
4. Register the merge driver (§9.2 step 2), `git add` the four files, and
   `git commit --quiet -m "init dev_sync workspace"`.
5. Report next steps: `git -C DIR remote add origin <workspace repo url>`, then
   `DIR/sync push`. Repos already inside `DIR` are recorded by the first push; to take the
   layout from another tree, use `import`.

### 9.4 `status` (no network, read-only)

Reports:

- **workspace:** branch; ahead/behind its upstream as of the last fetch (i.e. unpushed layout
  commits); merge in progress; uncommitted edits to `repos.toml`.
- **layout:** local changes not recorded yet (`detect` + `apply_to_snapshot` as a dry run,
  local conflicts included); pending repos; blocked removals with their reasons (runs
  `removal_safety`); local-only repos (info); leftover `.dev_sync-*` temp directories.
- **repos:** per repo, inspected in parallel: uncommitted changes, untracked files, stash,
  operation in progress, detached HEAD, branches ahead/behind/diverged/gone, branches with
  no upstream.
- **tool:** `.setup` with local changes, unpushed commits, or behind its upstream (last
  fetch).

Exit code 0 when nothing needs attention, 2 when something does, 1 on errors.

### 9.5 `pull`

1. Self-update (§8.11). (Removed, §19.)
2. Preamble (§9.2).
3. Scan, detect, apply to the snapshot, commit if changed, save the base (§8.2, §8.3, §8.12).
4. **Workspace upstream.**
   - The workspace branch has no upstream: if an `origin` remote exists, fetch it; if
     `refs/remotes/origin/<branch>` then exists, set it as upstream
     (`git branch --set-upstream-to=origin/<branch>`).
   - No `origin` at all → error "the workspace repo has no origin remote; add one with
     `git -C <root> remote add origin <url>`".
5. **Fetch** the upstream's remote under the network policy. On failure → report it with its
   kind and hint, change nothing else, exit 1. A layout commit from step 3 stays local and is
   merged next time.
6. The upstream branch doesn't exist yet (fresh, empty remote) → skip the merge with info
   "origin has no `<branch>` yet; `./sync push` creates it".
7. **Merge.** First compute the merge in-process for the report: base = snapshot at
   `git merge-base HEAD <upstream>`, local = HEAD's snapshot, incoming = the upstream's
   snapshot. Then run `git merge --no-edit --quiet <upstream full ref>`. The merge driver
   handles `repos.toml`; other files get git's normal merge.
   - exit 0 → merged (or already up to date, or fast-forwarded). Report the incoming layout
     changes.
   - non-zero and MERGE_HEAD exists → **conflicted.** List unmerged paths
     (`git diff --name-only --diff-filter=U -z`), print the layout conflicts from the
     in-process merge, and print "resolve repos.toml, then `./sync pull --continue` (or
     `./sync pull --abort`)". Other unmerged files: tell the user to resolve them with git.
     Exit 2.
   - non-zero without MERGE_HEAD → failure with git's message (e.g. local changes would be
     overwritten). Exit 1.
8. **Reconcile** the disk against HEAD's snapshot (§8.7, §8.8).
9. **Content pull** (§8.10).
10. Print the report and exit with its code.

**`pull --continue`:**

1. Requires MERGE_HEAD.
2. Parse the working-copy `repos.toml`. Remaining markers are an error that lists the lines.
3. Validate it, write it back canonically, and `git add repos.toml`.
4. If other unmerged paths remain → error listing them.
5. `git commit --no-edit --quiet`.
6. Then steps 8–10.

**`pull --abort`:** requires MERGE_HEAD; runs `git merge --abort` and reports it.

### 9.6 `push`

1. Preamble; scan, detect, record (§8.2, §8.3, §8.12).
2. **Content push** (§8.10) for every repo, including `.setup` (revised, §19: there is no
   `.setup`).
3. **Workspace push** under the network policy:
   - no upstream → requires `origin` (else the §9.5 error); run
     `git push --porcelain -u origin <branch>`;
   - otherwise `git push --porcelain <remote> <branch>:<upstream branch>`;
   - rejected → failure: "origin has layout changes you don't have — run `./sync pull`, then
     `./sync push`". Exit 1;
   - nothing to push → info.
4. Report blocked removals as attention. They **don't** block the push: they aren't in the
   layout, so nothing gets resurrected.
5. Exit with the report's code.

### 9.7 `keep <PATH>`

`PATH` must be `RemovalBlocked` in the state and present on disk with an origin. Add it to
the snapshot with its URL from disk, commit `<host>: keep <PATH>`, set its status to
`Synced`, and report "run `./sync push` to publish".

### 9.8 `import <DIR>`

Scan `DIR` with the same rules as §8.1; `DIR` needn't be a workspace, and paths are relative
to it.

- Every repo with an origin becomes an `Add` to the snapshot. An existing identical entry is
  skipped. A conflicting URL, or nesting, is an error that lists every problem and writes
  nothing.
- Local-only repos are reported.
- Commit `<host>: import N repos from <DIR>`.
- The next `pull` clones them (they're pending). `DIR` is never modified.

### 9.9 `merge-driver <BASE> <LOCAL> <INCOMING> <PATH>`

1. Read the three files. They're relative to the current directory, so never change the
   directory.
2. Parse each with the merge-input rules (§6.1; empty means an empty layout).
3. If any fails to parse → fall back to a text merge:
   `git merge-file -L local -L base -L incoming <LOCAL> <BASE> <INCOMING>` writes into
   `<LOCAL>`. Exit 1 if it reports conflicts (a non-zero result), else 0.
4. Otherwise `merge` (§8.5): `Clean` → write the canonical render into `<LOCAL>`, exit 0.
   `Conflicted` → write `render_conflicted` into `<LOCAL>`, exit 1.
5. Print nothing on success.

**Verified 2026-09-30 with git 2.54:**

- git calls a registered driver on `merge` **and** on `rebase`, passing `%O %A %B` as temp
  files relative to the repo root (e.g. `.merge_file_J4TLh8`) and `%P` as the path;
- a non-zero exit makes git report `CONFLICT`, mark the file `UU` and leave MERGE_HEAD;
- if the driver named in `.gitattributes` isn't registered in the config, git falls back to
  its normal text merge.

### 9.10 Output and exit codes

- The report goes to stdout at the end, grouped by scope in this order: workspace, layout,
  disk, repos (sorted by path), tool.
- Markers: `✓` done, `·` info, `!` needs attention, `✗` failed. One line per item, e.g.
  - `✓ cloned ferrisoft/website`
  - `! devman: main diverged from origin/main (2 ahead, 1 behind) — resolve with git in devman`
  - `✗ website: fetch failed (network, 3 attempts): Could not resolve host: github.com`
  - `· hetzner has no origin remote — it exists only on this machine`

  When a failure took more than one attempt, show the attempt count.
- Diagnostics go through `tracing` to stderr. The default level is `warn`; `--verbose`
  selects `debug`; the `DEV_SYNC_LOG` env var (EnvFilter syntax) overrides both. Log every
  git invocation at debug level: args, duration, exit status.
- Exit codes:

  | Code | Meaning |
  |---|---|
  | `0` | done, nothing needs the user |
  | `1` | something failed, or the command couldn't run |
  | `2` | done, but something needs the user (conflict, blocked removal, diverged branch, local conflict) |

  `merge-driver`: 0 clean, 1 conflict.
- Errors (`anyhow`) print as `error: {:#}` on stderr, exit 1. `main` returns `ExitCode`.

---

## 10. Git integration

### 10.1 Process runner (`process.rs`, knows nothing about git)

```rust
pub(crate) struct Finished { pub(crate) code: Option<i32>, pub(crate) stdout: Vec<u8>, pub(crate) stderr: Vec<u8> }
pub(crate) enum Completion { Finished(Finished), TimedOut { stderr: Vec<u8> } }
pub(crate) fn run(command: std::process::Command, timeout: std::time::Duration) -> anyhow::Result<Completion>;
```

- stdin is null; stdout and stderr are piped. Start one reader thread per pipe
  (`std::thread::spawn`, **not** scoped); each sends its buffer over a channel at EOF.
- The main thread polls `try_wait`, sleeping 1 ms and doubling up to 25 ms, until the child
  exits or the deadline passes. At the deadline: `kill()`, then `wait()`.
- Collect the buffers with a bounded wait (at most 2 s). Never join the readers without a
  bound: a killed git can leave an ssh child holding the pipe open.
- A spawn failure → `Err("failed to run git: … — is git installed?")`.
- Output larger than the pipe buffer (64 KiB) must not deadlock (test it).

### 10.2 Environment

Set these on each `Command`. **Never mutate the process environment.**

- `LC_ALL=C`: stable English messages for classification.
- `GIT_TERMINAL_PROMPT=0`: git never prompts for HTTPS credentials, so it fails fast.
- `GIT_OPTIONAL_LOCKS=0` on read-only queries, so `status` doesn't take `index.lock` and
  parallel queries are safe.
- Network commands only:
  - When neither `GIT_SSH_COMMAND` nor `GIT_SSH` is set, and `git config --get
    core.sshCommand` (checked once, in the root) is unset, set
    `GIT_SSH_COMMAND=ssh -o ConnectTimeout=20 -o ServerAliveInterval=15 -o ServerAliveCountMax=3`.
    For **parallel** operations (content fetch and push, clones) append ` -o BatchMode=yes`,
    so hidden interleaved prompts can't hang them. Sequential ones (the workspace
    fetch/push, self-update) leave BatchMode off, so an ssh host-key or passphrase prompt can
    still reach the terminal.
  - Add `-c http.lowSpeedLimit=1000 -c http.lowSpeedTime=60` to abort stalled HTTPS
    transfers.
- Don't override commit signing. If the user's git signs commits, layout commits are signed
  too.

### 10.3 Network policy

A `NetworkPolicy` struct built once in `main` and passed down:

| Setting | Value |
|---|---|
| fetch / push timeout | 300 s |
| clone timeout | 1800 s |
| local command timeout | 60 s (not network, but nothing may hang forever) |
| retries | only for `RemoteFailureKind::Network`: 3 attempts total, sleeping `base` then `2.5 × base` between them (`base` = 2 s) |

- No retry for `Timeout`, `Auth`, `NotFound`, `Rejected` or `Other`.
- `DEV_SYNC_NETWORK_TIMEOUT_SECS` overrides the fetch/push/clone timeouts, and
  `DEV_SYNC_RETRY_BASE_DELAY_MS` overrides `base`. Both exist for tests; an unparsable value
  is a startup error.
- Before retrying a clone, remove its temp directory.
- Parallelism: at most 8 concurrent network operations.

### 10.4 Failure classification

`classify(stderr: &str) -> RemoteFailureKind` matches on lowercase stderr (produced under
`LC_ALL=C`). Checks run in this order; the first match wins:

1. **Rejected:** `[rejected]`, `[remote rejected]`, `non-fast-forward`, `fetch first`,
   `updates were rejected`
2. **Auth:** `permission denied (publickey`, `permission denied, please try again`,
   `authentication failed`, `could not read username`, `could not read password`,
   `terminal prompts disabled`, `host key verification failed`, `returned error: 401`,
   `returned error: 403`, `invalid username or password`
3. **NotFound:** `repository not found`, `does not appear to be a git repository`,
   `returned error: 404`, `couldn't find remote ref`, `no such remote`
4. **Network:** `could not resolve host`, `could not resolve hostname`,
   `temporary failure in name resolution`, `name or service not known`,
   `network is unreachable`, `no route to host`, `connection timed out`,
   `connection refused`, `connection reset`, `operation timed out`, `failed to connect to`,
   `couldn't connect to server`, `the remote end hung up unexpectedly`, `early eof`,
   `rpc failed`, `unexpected disconnect`, `connection closed by`,
   `kex_exchange_identification`, `broken pipe`, `ssl_error`, `gnutls`, `tls connection`
5. **Other**

`Timeout` is assigned by the runner when it kills the process at the deadline; `classify`
never returns it. Keep the last three non-empty stderr lines as `detail`.

Hints for the report:

| Kind | Hint |
|---|---|
| Network | "check the connection; running it again retries" |
| Timeout | "no answer within N s" |
| Auth | "check ssh-agent (`ssh-add -l`) and access to the repo; test with `ssh -T git@github.com`" |
| NotFound | "the repo or branch doesn't exist, or you lack access" |
| Rejected | "the remote has commits you don't have — pull first" |

### 10.5 Command reference

| Purpose | Command |
|---|---|
| origin URL | `git -C <repo> config --get remote.origin.url` (exit 1 = none) |
| HEAD, working tree, current upstream | `git -C <repo> status --porcelain=v2 --branch -z --untracked-files=normal` |
| all branches + upstreams | `git -C <repo> for-each-ref --format=%(refname:short)%00%(upstream)%00%(upstream:remotename)%00%(upstream:remoteref)%00%(upstream:track,nobracket) refs/heads` |
| stash present | `git -C <repo> rev-parse --verify --quiet refs/stash` |
| operation in progress | for `MERGE_HEAD`, `rebase-merge`, `rebase-apply`, `CHERRY_PICK_HEAD`, `REVERT_HEAD`, `BISECT_LOG`: `git -C <repo> rev-parse --git-path <name>`, then check whether it exists (a relative result is relative to `<repo>`) |
| unpushed commits | `git -C <repo> rev-list --count --branches --not --remotes`; detached: `git -C <repo> rev-list --count HEAD --not --remotes` |
| worktrees | `git -C <repo> worktree list --porcelain` |
| fetch | `git -C <repo> fetch --all --prune --quiet` |
| fast-forward | `git -C <repo> merge --ff-only --quiet @{upstream}` |
| push | `git -C <repo> push --porcelain <remote> refs/heads/<b>:<remote_ref> …` |
| clone | `git clone --quiet -- <url> <temp dir>` |
| set URL | `git -C <repo> remote set-url origin <url>` |
| worktree repair | `git -C <repo> worktree repair [<dir>…]` |
| workspace: current branch | `git -C <root> symbolic-ref --quiet --short HEAD` (exit 1 = detached) |
| workspace: file at a commit | `git -C <root> show <rev>:repos.toml` |
| workspace: merge base | `git -C <root> merge-base HEAD <upstream>` |
| workspace: repos.toml modified? | `git -C <root> status --porcelain=v2 -z -- repos.toml` |
| workspace: commit layout | `git -C <root> commit --quiet -m <msg> -- repos.toml` |
| workspace: merge | `git -C <root> merge --no-edit --quiet <upstream full ref>` |
| workspace: unmerged paths | `git -C <root> diff --name-only --diff-filter=U -z` |
| workspace: git dir | `git -C <root> rev-parse --absolute-git-dir` |

Always put `--` before user-controlled positional arguments (paths, URLs) where git accepts
it.

### 10.6 Parsing notes (the parsers are pure functions with table tests on real samples)

- **`status --porcelain=v2 -z`:** records end with NUL.
  - Headers start with `# `: `# branch.oid <commit>|(initial)`,
    `# branch.head <branch>|(detached)`, `# branch.upstream <upstream>`,
    `# branch.ab +<ahead> -<behind>`.
  - Entries: `1 ` changed, `2 ` renamed/copied, `u ` unmerged, `? ` untracked, `! ` ignored
    (not requested).
  - **A `2` record is followed by one extra NUL-terminated field (the original path) that
    must be consumed**, or every later record is misread.
- **`for-each-ref`** (format above): one line per branch. The track field is empty,
  `ahead N`, `behind N`, `ahead N, behind M`, or `gone`. Parse the counts into `u32` with
  checked parsing; bad input is an error, never a panic or overflow.
- **`worktree list --porcelain`:** records separated by a blank line: `worktree <path>`, then
  `HEAD <sha>`, then `branch refs/heads/<b>` | `detached` | `bare`, optionally `locked` and
  `prunable <reason>`. The first record is the main worktree.
- **`push --porcelain`** (stdout): lines of the form `<flag>\t<from>:<to>\t<summary>`, where
  flag is ` ` (fast-forward), `+` (forced), `-` (deleted), `*` (new), `!` (rejected), `=`
  (up to date), plus `To <url>` and `Done` lines to skip.

---

## 11. Error handling and safety rules

- **`anyhow`** for errors, with `.context(...)` naming the repo, path and operation. A
  user-facing message says what failed, where, and what to do.
- **Expected outcomes are values, not errors:**
  - network, auth or rejection → `RemoteOutcome::Failed`;
  - conflicts → `MergeOutcome::Conflicted` / `Recorded::Conflicts`;
  - an unsafe removal → `RemovalSafety::Unsafe`.

  `Err` means "this command can't proceed": git missing, an unreadable directory, a corrupt
  state file, an invalid `repos.toml`, the lock held.
- **No panics.** None of `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!` or
  `unreachable!`; no indexing or slicing that can panic (use `get`); checked arithmetic on
  every parsed number; map a poisoned `Mutex` to an error. Enforced by clippy lints (§12.3).
  Tests return `anyhow::Result<()>` and use `?`.
- **Partial failure.** One repo's failure never stops the others. Everything is reported and
  the exit code is 1.
- **Abort before side effects** when inputs are inconsistent: scan error, local conflict,
  invalid `repos.toml`, merge in progress, lock held.
- **Never lose user data.** Removals go to the Trash, and only after the safety check. The
  only directories the tool deletes are its own clone temps (`.dev_sync-cloning-*`) and
  empty directories. Move temps are never deleted.
- **Crash safety.** The state file is written atomically, and every step is idempotent, so a
  run killed at any point converges on the next run.
- **No `std::process::exit`** outside `main`.
- **No `unsafe`** (`unsafe_code = "forbid"`).

---

## 12. Code structure, dependencies, toolchain, lints

### 12.1 Module tree

Follow the user's rust-guidelines: a `module.rs` file next to a `module/` directory, never
`mod.rs`; a parent module only re-exports its children.

```
Cargo.toml  Cargo.lock  CLAUDE.md  .gitignore (/target)
docs/design.md
templates/gitignore  templates/gitattributes  templates/sync
src/
  main.rs               parse CLI, init tracing, build NetworkPolicy, dispatch, print report, ExitCode
  cli.rs                clap types
  domain.rs             re-exports ↓
  domain/repo_path.rs  domain/remote_url.rs  domain/file_id.rs  domain/host_name.rs  domain/git_names.rs (BranchName, CommitId)
  layout.rs             re-exports ↓
  layout/model.rs  layout/change.rs  layout/merge.rs  layout/file.rs
  process.rs            timed child processes (§10.1)
  git.rs                re-exports ↓
  git/runner.rs         Git: env, local queries, network ops with policy + retries
  git/failure.rs        RemoteFailure(Kind), RemoteOutcome, classify
  git/parse.rs          porcelain v2, for-each-ref, worktree list, push porcelain parsers
  git/repo.rs           typed queries: origin, inspect (RepoStatus), operation, stash, unpushed counts, worktrees
  scan.rs               §8.1
  state.rs              MachineState load/save (§6.2), lock guard (§6.3)
  record.rs             detect, apply_to_snapshot, next_base (§8.2, §8.3, §8.12)
  reconcile.rs          re-exports ↓
  reconcile/plan.rs     pure planner (§8.7)
  reconcile/execute.rs  executor (§8.8)
  safety.rs             removal_safety (§8.9)
  content.rs            fetch/inspect, pull_decision, push_plan, execution (§8.10)
  self_update.rs        §8.11
  workspace.rs          Workspace (root, paths), discovery, templates, init, merge-driver registration, preamble checks
  report.rs             Report, Item, Severity, Scope, rendering, exit code
  commands.rs           re-exports ↓
  commands/init.rs  commands/status.rs  commands/pull.rs  commands/push.rs  commands/keep.rs  commands/import.rs  commands/merge_driver.rs
tests/
  e2e.rs                end-to-end scenarios (§13.2)
  e2e/support.rs        World/Machine harness (`mod support;` in e2e.rs)
```

### 12.2 Dependencies

Look up the latest stable versions (`cargo search <name>`; the `trash` crate was 5.2.9 on
2026-09-30). Start every dependency with `default-features = false` and enable only what's
needed:

- `anyhow` (std)
- `clap` (std, derive, help, usage, error-context)
- `serde` (std, derive)
- `toml`: parse and deserialize `repos.toml`, and parse and serialize `state.toml`. Check the
  crate's current feature names.
- `tracing` (std) and `tracing-subscriber` (std, fmt, env-filter, ansi)
- `trash`: check which features Linux needs with the defaults off.
- dev-dependency: `tempfile`

Nothing else without a stated reason: no `rayon` (use `std::thread::scope`), no `git2`, no
`libc`/`nix` (`std::os::unix` covers inode, exec and permissions).

### 12.3 Toolchain and lints

- **Stable Rust 1.95** (the nixpkgs cargo; this machine has no rustup), edition 2024. No
  `rust-toolchain.toml`, no unstable features. The user's guideline prefers nightly, but none
  is installed; this is listed in §18.
- In `Cargo.toml`:

```toml
[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
panic = "deny"
todo = "deny"
unimplemented = "deny"
unreachable = "deny"
indexing_slicing = "deny"
string_slice = "deny"
exit = "deny"
use_self = "warn"
```

- `cargo clippy --all-targets -- -D warnings` must be clean. Never `#[allow(...)]` in
  production code.
- Don't run rustfmt unless the repo has a `rustfmt.toml` (rust-guidelines). Before starting,
  read the user's existing Rust tools (read-only) to mirror their conventions:
  `/home/wdanilo/dev/shell/tools/fido-unlock` (and `fprint-unlock`, `spotify-auth`):
  `Cargo.toml`, `CLAUDE.md`, lint setup.

### 12.4 Style (from the user's rust-guidelines; the session hook loads the full skill)

- A section header per logical unit (`// === Name ===` with matching `=` lines).
- Doc comments only for non-obvious things, never describing call sites.
- Newtypes over bare primitives; exhaustive `match`; `#[must_use]` on outcome types.
- A single return point preferred; functional style (iterators, expressions).
- Qualified names over leaf imports; one `use` per line, sorted.
- Field-init shorthand; derives in alphabetical order; trailing commas; lines at most 120
  characters.
- Every `Cargo.toml` has a sibling `CLAUDE.md`.

---

## 13. Testing

TDD: write the test, watch it fail for the expected reason, implement, watch it pass, then
run the **whole** suite.

### 13.1 Unit tests (in-file `#[cfg(test)] mod tests`)

**`RepoPath`**

- Accepts: `a`, `b/c`, `ferrisoft/design_system`, `zażółć/x`, `my repo/x`, `a"b`, `a\b`.
- Rejects: ``, `/a`, `a/`, `a//b`, `.`, `..`, `a/../b`, `a/./b`, `.hidden`, `a/.git`, and a
  string containing `\n` or `\0`.
- `overlaps`: `a`/`a/b` true, `a`/`ab` false, `a/b`/`a/b` true, `a-b`/`a/b` false.
- `proper_ancestors("a/b/c") == [a, a/b]`.

**`RemoteUrl`**

- Accepts `git@github.com:o/r.git`, `https://github.com/o/r.git`, `/tmp/x/r.git`.
- Rejects ``, ` x`, `x `, `-oProxyCommand=x`, and a string containing `\n`.

**`Layout`:** `from_repos` rejects duplicates and `a` + `a/b`, and accepts siblings.
`apply`:

- batch swap `a↔b` works;
- a chain `a→b`, `b→c` works;
- each precondition failure yields `Rejected` with the right reason, and the input is left
  unchanged.

**`diff`** (paths `a`, `b`, `x`; URLs `u1`–`u3`):

- add, remove, set-url;
- `{a:u1}` → `{x:u1}` is `Move a→x`;
- `{a:u1, b:u1}` → `{a:u1, x:u1}` is exactly `Move b→x`;
- output is deterministic across runs.

**`merge`** (base / local / incoming → expected):

| # | base | local | incoming | expected |
|---|---|---|---|---|
| 1 | {} | {a:u1} | {b:u2} | Clean {a:u1, b:u2} |
| 2 | {} | {a:u1} | {a:u1} | Clean {a:u1} |
| 3 | {} | {a:u1} | {a:u2} | Conflicted; resolved {}; local_repos [a:u1], incoming_repos [a:u2] |
| 4 | {a:u1} | {} | {} | Clean {} |
| 5 | {a:u1, b:u2} | {b:u2} | {a:u1, b:u2} | Clean {b:u2} |
| 6 | {a:u1} | {} | {a:u2} | Conflicted (local side empty) |
| 7 | {a:u1} | {x/a:u1} | {a:u1} | Clean {x/a:u1} |
| 8 | {a:u1} | {x:u1} | {y:u1} | Conflicted, one group; local_repos [x:u1], incoming_repos [y:u1] |
| 9 | {a:u1} | {x:u1} | {x:u1} | Clean {x:u1} |
| 10 | {a:u1} | {x:u1} | {} | Conflicted (move vs remove) |
| 11 | {} | {a:u1} | {a/b:u2} | Conflicted (overlap) |
| 12 | {a:u1} | {a:u2} | {a:u2} | Clean {a:u2} |
| 13 | {a:u1} | {a:u2} | {a:u3} | Conflicted |
| 14 | {a:u1, b:u2} | {b:u2, c:u1} | {a:u1, c:u2} | one group (a, b, c); resolved {} |
| 15 | {a:u1} | {a:u1, b:u2, c:u3} | {a:u1, c:u4, d:u5} | Conflicted on c only; resolved {a:u1, b:u2, d:u5} |
| 16 | {a:u1, b:u1} | {a:u1, x:u1} | {a:u1, b:u1} | Clean {a:u1, x:u1} |

**`repos.toml`**

- Render → parse round-trip, including paths and URLs with spaces, quotes, backslashes and
  non-ASCII.
- Rendering is byte-stable, sorted and ends with one `\n`.
- Errors:
  - missing `format`;
  - `format = 2` gives the "newer format" message;
  - an unknown top-level key; an unknown entry key;
  - an invalid path; nested entries;
  - conflict markers, reporting the right line numbers.
- Empty merge input gives an empty layout.
- The output of `render_conflicted` contains the markers and is refused by `parse`.
- Resolving that output by keeping one side parses into the expected layout.

**`classify`:** a table of real stderr samples → kind. At least:

| stderr sample | kind |
|---|---|
| `ssh: Could not resolve hostname github.com: Temporary failure in name resolution` + `fatal: Could not read from remote repository.` | Network |
| `git@github.com: Permission denied (publickey).` | Auth |
| `ERROR: Repository not found.` | NotFound |
| `fatal: unable to access 'http://127.0.0.1:9/x.git/': Failed to connect to 127.0.0.1 port 9 after 0 ms: Couldn't connect to server` | Network |
| `fatal: could not read Username for 'https://github.com': terminal prompts disabled` | Auth |
| ` ! [rejected]        main -> main (fetch first)` | Rejected |
| `fatal: '/tmp/nope' does not appear to be a git repository` | NotFound |
| `fatal: the remote end hung up unexpectedly` | Network |
| an unrelated message | Other |

**Parsers:** real porcelain v2 samples:

- a clean repo;
- modified, staged, untracked, unmerged entries;
- **a rename record with its extra original-path field, followed by another record**;
- `(initial)`, `(detached)`, upstream and `ab` headers.

`for-each-ref` track strings: ``, `ahead 3`, `behind 2`, `ahead 1, behind 4`, `gone`, and
an overflowing number (an error). A worktree list with a main worktree, a branch worktree, a
detached worktree and a prunable one. A push porcelain sample with one ok ref and one
rejected ref.

**`detect`** (base, observed → changes):

1. unchanged → [];
2. new → Add;
3. gone → Remove;
4. same id, new path, same URL → Move;
5. same id, new path, new URL → Remove + Add;
6. same path, new id, same URL → [];
7. same id and path, new URL → SetUrl;
8. no origin and not in base → local_only;
9. in base, origin now missing → error;
10. `{a:u1 id1, b:u1 id2}` with b moved to x → Move b→x only;
11. blocked and still on disk → [];
12. blocked and deleted → Remove.

**`apply_to_snapshot`:** every row of the §8.3 table.

**`plan`** (base, target, observed, facts → actions/conflicts):

1. pending clone;
2. adopt;
3. occupied by another clone → conflict;
4. remove;
5. move;
6. swap: base {a:u1, b:u2} → target {a:u2, b:u1} gives two moves, not two set-urls;
7. chain: base {a:u1, b:u2} → target {b:u1, c:u2} gives moves a→b and b→c;
8. set-url: base {a:u1} → target {a:u2} with no other u1/u2 entries gives SetUrl;
9. clone into a non-empty non-repo directory → Occupied;
10. clone under an existing repo → Nested;
11. a blocked entry absent from the target → Remove (retried).

**Decisions:** every row of the `pull_decision` and `push_plan` tables in §8.10; every branch
of `self_update_decision`; `Report::exit_code` for combinations of severities.

**Process runner:**

- a quick command → `Finished` with its code and output;
- `sleep 5` with a 200 ms timeout → `TimedOut` in well under 1 s;
- 1 MiB of output doesn't deadlock;
- a missing program → `Err`.

**With real git in tempdirs** (a `git init` helper using an isolated config, see §13.3):

- scanner: hidden folders skipped; a `.git` file skipped; no descent into repos; symlinks not
  followed; local-only reported; an unreadable directory is an error (skip the test when
  running as root); a non-UTF-8 name is an error;
- `removal_safety`: `Safe` for a clean, pushed clone, and one test per `UnsafeReason`;
- state: load/save round trip, a missing file, a bad format, atomic replace, lock taken /
  held by a live PID / stale;
- execution: a move with worktree repair (a repo with a linked worktree under
  `.claude/worktrees/x` moves, and afterwards `git -C <new>/.claude/worktrees/x status`
  works and `git worktree list` shows the new path); a two-phase swap; a clone through the
  temp directory; clone failure leaving no temp behind.

Removals through the real Trash are covered end-to-end (§13.2), where `XDG_DATA_HOME` can be
set per process. Don't add test-only code paths to production code.

### 13.2 End-to-end tests (`tests/e2e.rs`, run the binary via `env!("CARGO_BIN_EXE_dev_sync")`)

**Harness (`tests/e2e/support.rs`):**

- `World` owns a tempdir holding `remotes/` (bare repos standing in for GitHub, created with
  one commit on `main`, plus an empty bare `dev2.git` for the workspace) and a generated
  gitconfig.
- `World::machine(name)` returns a `Machine` with its own `home`, `xdg-data`, `xdg-cache` and
  `dev2` directories.
- `Machine::dev_sync(args)` runs the binary with an isolated env (§13.3) plus
  `DEV_SYNC_HOST=<name>` and returns exit code, stdout and stderr.
- `Machine::git(dir, args)` runs git with the same env.

A typical setup: laptop `init` → `git remote add origin <remotes/dev2.git>` → work → `push`;
demeter `git clone <remotes/dev2.git> dev2` → `pull`.

**Scenarios.** Each is one test; assert on the filesystem, git state and exit codes, and on
key report phrases only.

1. `init` writes the four files and makes the initial commit; the merge driver config is
   set; `status` on an empty workspace exits 0.
2. A clone added on the laptop, `push` → the layout commit subject is
   `laptop: +ferrisoft/x` → demeter's `pull` clones it at the same nested path.
3. Move on the laptop (`mv`), push → demeter's pull moves it; a local unpushed commit in
   demeter's clone survives at the new path.
4. Remove on the laptop (`rm -rf` in the test), push → demeter's clean clone ends up in
   `$XDG_DATA_HOME/Trash/files/`.
5. **Blocked removal:** demeter's clone has an unpushed commit → pull exits 2 and the clone
   stays; `status` reports it; after `keep`, push, and a laptop pull, the laptop has it again.
6. **Blocked, then resolved:** demeter pushes the work (`./sync push` pushes that repo's
   branch), pulls again → now trashed.
7. URL change on the laptop (`git remote set-url`), push → demeter's clone gets the new
   origin URL.
8. **Conflict:** both machines add the same path with different URLs → the second pull exits
   2 and `repos.toml` has markers; `pull --continue` without resolving fails; resolving by
   keeping one side makes `--continue` succeed and clone it; a separate test covers
   `--abort`.
9. **Clean concurrent changes:** laptop adds `x` and demeter adds `y` → demeter's pull merges
   without conflict (the driver ran: no markers, both entries present).
10. **Content pull:** the laptop commits and pushes in repo `r` → demeter's pull
    fast-forwards `r`; with local and remote commits it's diverged (exit 2, branch
    untouched); dirty and behind → not fast-forwarded, reported.
11. **Content push:** a commit on a branch with an upstream is pushed (the bare remote has
    it); a branch without an upstream is reported and not pushed.
12. **Workspace remote unreachable** (`http://127.0.0.1:<closed port>/x.git`) → pull exits 1
    with "(network, 3 attempts)"; the local layout commit exists; the disk is unchanged.
13. **One repo unreachable among several:** the others are cloned, exit 1; after the URL is
    fixed, the next pull clones the missing one.
14. **Timeout:** a TCP listener that accepts and never answers, with
    `DEV_SYNC_NETWORK_TIMEOUT_SECS=2` → reported as a timeout within a few seconds; the
    listener thread is shut down at test end.
15. **Auth:** a minimal HTTP responder that always returns
    `401 Unauthorized` + `WWW-Authenticate: Basic realm="x"` → reported as auth, with no
    retry (no "attempts" in the line).
16. A local-only repo → reported with `·`, absent from `repos.toml`.
17. `merge-driver` called directly on three files → exit 0 with the merged result, and exit 1
    with markers for a conflict; an unparsable input falls back to the text merge.
18. **Self-update:**
    - `<root>/sync` is a fake script that writes a marker file and exits 0;
    - `<root>/.setup` is a clone of a small bare repo that has a newer commit upstream;
    - `dev_sync --root <root> pull` → `.setup` is fast-forwarded and the marker exists,
      i.e. the re-exec happened with `DEV_SYNC_SELF_UPDATED=1`;
    - with local changes in `.setup` → no update, info reported.
19. **`import`:** repos in another tree (nested, one local-only) are recorded; `pull` clones
    them; the source tree is unchanged.
20. **Lost state is harmless:** after a sync, delete `.git/dev_sync/state.toml` → `push`
    records nothing new, removes nothing, and exits 0.
21. **Unusual paths:** the whole world lives under a directory containing a space and a `'`,
    and one repo path contains a space and non-ASCII → every scenario above that touches
    paths still works: launcher quoting, driver command quoting, TOML quoting.
22. **Child repo mid-rebase** (a stopped `git rebase -i` or a conflicted merge) whose repo was
    removed on the other machine → blocked with "operation in progress"; not
    fast-forwarded.
23. **Interrupted runs:** a leftover `.dev_sync-cloning-*` directory is removed on the next
    pull; a leftover `.dev_sync-moving-*` directory is reported and kept.
24. **Lock:** a lock file holding a live PID (the test's own) → the command errors; a lock
    holding a dead PID → taken over.
25. **Workspace preconditions:** a detached workspace HEAD → error; `repos.toml` edited
    without committing → error; `.gitattributes` deleted → a merge still works through git's
    text fallback, and `--continue` validates.

### 13.3 Isolation rules

- **Never touch** `~/dev`, `~/dev2`, the real Trash, the user's git config, or the network
  beyond 127.0.0.1.
- Every subprocess (binary and helper git) gets:
  - `HOME=<tmp>/home`;
  - `GIT_CONFIG_GLOBAL=<generated file>` with `user.name`, `user.email`,
    `init.defaultBranch=main`, `commit.gpgsign=false`, `tag.gpgsign=false`;
  - `GIT_CONFIG_NOSYSTEM=1`;
  - `XDG_DATA_HOME` and `XDG_CACHE_HOME` under the tempdir;
  - `DEV_SYNC_NETWORK_TIMEOUT_SECS` (small) and `DEV_SYNC_RETRY_BASE_DELAY_MS=10`;
  - `env_remove("GIT_SSH_COMMAND")`.
- Remotes are local bare repos addressed by path.
- Failure servers bind `127.0.0.1:0`. A closed port comes from binding and then dropping a
  listener. Every server stops at test end, so no threads or processes outlive the test.
- Tests are parallel-safe: no global env mutation, no fixed ports, no shared directories.
- Unit tests that need repos create them with `git init` in a tempdir, with the same env.

---

## 14. Implementation plan

**Execution:** the implementing session carries out these tasks itself, in order, then runs
the review in §15. Every task follows TDD (§13): tests first, watch them fail, implement,
watch the whole suite pass.

### Global constraints

- Stable Rust 1.95, edition 2024, binary crate `dev_sync`; lints exactly as in §12.3;
  `cargo clippy --all-targets -- -D warnings` clean at the end of every task.
- Dependencies only from §12.2, all with `default-features = false`.
- No panics, no `unsafe`, no process-wide env mutation; `status` never uses the network.
- File names and paths are fixed: `repos.toml`, `sync`, `.setup`, `.git/dev_sync/state.toml`,
  `.git/dev_sync/lock`, `.dev_sync-cloning-*`, `.dev_sync-moving-*`, merge driver
  `dev-sync`.
- Env vars: `DEV_SYNC_HOST`, `DEV_SYNC_LOG`, `DEV_SYNC_NETWORK_TIMEOUT_SECS`,
  `DEV_SYNC_RETRY_BASE_DELAY_MS`, `DEV_SYNC_SELF_UPDATED`.
- Exit codes 0/1/2 as in §9.10.

### Review focus

Five cases the spec implies that are the most likely to hurt the user. Each has its own test
in §13.2:

1. A child repo in an unusual state (mid-rebase or mid-merge, detached, unborn, dirty
   worktrees) must never be trashed, fast-forwarded or pushed wrongly (scenario 22 plus the
   `removal_safety` unit tests).
2. Paths and URLs with spaces, quotes, backslashes and non-ASCII must survive `repos.toml`,
   `state.toml`, the launcher and the merge driver command (scenario 21).
3. A run interrupted at any point must converge on the next run without losing data
   (scenarios 20, 23).
4. Concurrent or hand-edited workspaces must fail clearly: two runs at once, `repos.toml`
   edited by hand, a missing `.gitattributes`, a detached HEAD (scenarios 24, 25).
5. Flaky or slow networks during multi-repo operations: partial failure, retries, timeouts,
   auth (scenarios 12–15).

### Tasks

**Task 1: Scaffold and tooling check**

- Files: `Cargo.toml`, `src/main.rs`, `.gitignore`, `CLAUDE.md`, `templates/`.
- Steps:
  1. Read the user's existing tool conventions (§12.3), read-only.
  2. `cargo init --vcs git --name dev_sync` in `~/dev/dev_sync` (the directory already holds
     `docs/`).
  3. Look up the latest crate versions, add the dependencies and lints, and `cargo build`.
  4. **Verify rust-analyzer** (the user's rust-guidelines make it mandatory): load the `LSP`
     tool (`ToolSearch` with `select:LSP`) and hover over `main` in `src/main.rs`. The first
     call can take a minute or two while the workspace loads. If it doesn't work, **stop and
     tell the user**. Troubleshooting: `~/.claude/skills/rust-analyzer-gate/`.
  5. Write a first `CLAUDE.md`: purpose, build/test/clippy commands, pointer to this file.

**Task 2: Domain types** (`domain/*`). Produces `RepoPath`, `RemoteUrl`, `FileId`,
`HostName`, `BranchName`, `CommitId` (§7). Tests: §13.1 tables.

**Task 3: Layout model, apply and diff** (`layout/model.rs`, `layout/change.rs`). Produces
`Layout`, `Entry`, `LayoutRepo`, `Applied`, `ChangeRejection`, `Change`, `diff` (§7, §8.4).
Tests: §13.1.

**Task 4: Three-way merge** (`layout/merge.rs`). Produces `merge`, `MergeOutcome`,
`ConflictedMerge`, `Conflict` (§8.5). Tests: the 16-row table.

**Task 5: `repos.toml` format** (`layout/file.rs`). Produces `parse`, `parse_merge_input`,
`render`, `render_conflicted` (§6.1, §8.6). Tests: §13.1.

**Task 6: Process runner** (`process.rs`). Produces `run`, `Completion`, `Finished` (§10.1).
Tests: §13.1.

**Task 7: Classification and parsers** (`git/failure.rs`, `git/parse.rs`). Produces
`classify`, `RemoteFailure`, `RemoteFailureKind`, `RemoteOutcome`, and the status /
for-each-ref / worktree / push parsers with their types (§7, §10.4, §10.6). Tests: sample
tables.

**Task 8: Git runner and network policy** (`git/runner.rs`, `git/repo.rs`). Produces `Git`,
`NetworkPolicy`, local query helpers, network operations with timeout and retries, and typed
repo queries (§10.2, §10.3, §10.5). Tests with real git: a query works; a non-zero exit is an
error; a closed port gives Network with 3 attempts; a missing path gives NotFound with 1
attempt; a stalled listener gives Timeout.

**Task 9: Scanner** (`scan.rs`, §8.1). Tests: §13.1.

**Task 10: Machine state and lock** (`state.rs`, §6.2, §6.3). Tests: §13.1.

**Task 11: Record** (`record.rs`: `detect`, `apply_to_snapshot`, `next_base`, §8.2, §8.3,
§8.12). Tests: the `detect` list and the `apply_to_snapshot` table.

**Task 12: Removal safety** (`safety.rs`, §8.9). Tests with real repos, one per reason.

**Task 13: Reconcile** (`reconcile/plan.rs`, `reconcile/execute.rs`, §8.7, §8.8). Tests:
the `plan` list; execution tests for the move with worktree repair, the swap, and clone
through a temp.

**Task 14: Content sync** (`content.rs`, §8.10). Tests: the decision tables, plus
fetch/fast-forward/push against local bare repos.

**Task 15: Workspace** (`workspace.rs`: discovery, templates, `init`, merge driver
registration, preamble checks; §6.4, §9.1–9.3). Tests: discovery from a subdirectory;
`init` refusals; driver command selection (launcher vs current exe) and quoting.

**Task 16: Report, CLI and commands** (`report.rs`, `cli.rs`, `commands/*`, `main.rs`,
§9.4–9.10). Tests: E2E scenarios 1–13 and 16–25.

**Task 17: Self-update** (`self_update.rs`, §8.11). Tests: decision unit tests and E2E
scenario 18.

**Task 18: Timeouts and auth end-to-end** (scenarios 14, 15), then polish:

1. `cargo test` fully green; clippy clean.
2. Update `CLAUDE.md` (module map, commands, env vars, test strategy, gotchas).
3. Record deviations in §19.
4. Measure `status` on a scratch workspace in the session scratchpad created with
   `init` + `import ~/dev` (import only reads `~/dev`), and quote the numbers.
5. Optionally, if network access works, `pull` into that scratch workspace to clone the real
   repos (about 100 MB), check the result, then delete the scratch workspace.

**Task 19: Review** (§15), triage and fixes.

**Task 20: Final report to the user:**

- what was built, the module map, test counts and timing;
- review outcomes (fixed, rejected with reason, deferred);
- deviations;
- the open questions from §18;
- the next steps for the user: create the two GitHub repos, `init ~/dev2`, and fill it.

**Commits:** the user's standing rule is to commit only when asked. Unless the user says
otherwise at the start of the implementing session, don't commit. If they allow commits,
make one per task with a descriptive message and no attribution lines. Never push.

---

## 15. Review with five independent agents

When Tasks 1–18 are done (tests green, clippy clean), launch **five reviewers in parallel**
in one message:

- Use the Agent tool with `subagent_type: "opus-max"`: a clean-context agent at maximum
  reasoning effort, the user's choice for reviews since 2026-09-30.
- Pass `model: "opus"` so it runs on Opus 5.5 whatever the session model is.

Each prompt is self-contained and says:

- the repo is `~/dev/dev_sync`, a Rust CLI; read `docs/design.md` first, then the code;
- **don't edit any file**;
- report findings as a list, each with: file:line, severity (critical/major/minor), a
  concrete failure scenario (inputs or state → wrong result), and confidence;
- verify each finding against the code, and by running tests or small experiments in a
  tempdir where cheap, before reporting it;
- report no style nits unless they break the user's rust-guidelines;
- each reviewer's focus is one of the five below.

**Focus areas:**

1. **Sync correctness and data safety:** record, diff, merge, reconcile, state. Can any
   sequence of operations across two machines lose work, resurrect a removed repo, clone
   into the wrong place, corrupt `repos.toml` or the state file, or leave the disk not
   matching the layout?
2. **Failure handling and robustness:**
   - network: timeouts, retries, classification, partial failure;
   - processes: hangs, zombies, pipe handling;
   - crash safety and idempotency;
   - no panics: hunt down every possible one.
3. **Type design and rust-guidelines compliance:**
   - are invariants enforced by types?
   - newtypes and enums;
   - module structure, headers, docs, imports;
   - dependency features, the clippy setup.
4. **Git integration:**
   - command choices and parsing (porcelain v2 `-z` including rename records,
     for-each-ref, worktree list, push porcelain);
   - env vars; quoting and argument injection (`--`);
   - merge driver behavior (relative paths, fallback); worktree repair;
   - unusual repo states: unborn, detached, in-progress operations, submodules, shallow
     clones.
5. **Tests and UX:**
   - would the tests fail if the code regressed (think in mutations)?
   - coverage of the §13.2 scenarios; isolation and flakiness;
   - user-facing messages, exit codes;
   - do `CLAUDE.md` and this file match the code?

**Then triage.** Verify every finding yourself; don't trust a reviewer blindly. Reproduce it
with a failing test where possible, and fix confirmed findings the TDD way. Rerun the full
suite and clippy, and include the outcome of every finding in the final report: fixed,
rejected (why), or deferred (why).

---

## 16. Rules for the implementing session

- **Work only in `~/dev/dev_sync` and the session scratchpad.** Everything else is
  read-only.
- **Don't** create `~/dev2`, create GitHub repos, add remotes, or push anything. Don't modify
  the repos in `~/dev`, `/etc/nixos`, or the dotfiles.
- **Kill every process you start.** Test servers must stop; leave no background jobs.
- **The Bash tool runs zsh.** Never name a shell variable `path`: in zsh it's tied to
  `$PATH`, and assigning it wipes the PATH mid-script. For multi-line scripts, use bash
  explicitly (`bash <<'EOF' … EOF`).
- **NixOS:** get extra tools with `nix shell nixpkgs#<pkg>` or `nix-shell -p`. No pip, no
  npm, no global `cargo install`.
- **Keep this file current:** record every deviation in §19 with the reason.
- **Non-goals for v1:**
  - per-machine subsets and ignore lists (the escape hatch is a hidden folder);
  - submodules (no `--recurse-submodules`); LFS specifics; shallow-clone handling beyond
    reporting;
  - publishing new branches, force-push, merging or rebasing repo contents;
  - Windows and macOS; a daemon or watch mode; any GUI;
  - managing the tool's own GitHub repo.

---

## 17. Environment facts (2026-09-30)

- git 2.54.0; cargo and rustc 1.95.0 (nixpkgs build, no rustup); vcs2l 1.1.7 in nixpkgs.
- The merge driver behavior in §9.9 was verified in a throwaway repo.
- `~/dev` after the user reorganized it on 2026-09-30: 10 clones. **Read-only reference
  data.**

| path | origin |
|---|---|
| `account_manager` | `git@github.com:ferrisoft/account_manager.git` |
| `claude-status` | `git@github.com:ferrisoft/account_manager.git` (same URL, second clone) |
| `devman` | `git@github.com:ferrisoft/devman.git` |
| `shell` | `git@github.com:ferrisoft/shell.git` |
| `ferrisoft/design_system` | `git@github.com:ferrisoft/design-system.git` |
| `ferrisoft/setup` | `git@github.com:ferrisoft/handbook.git` |
| `ferrisoft/website` | `git@github.com:ferrisoft/website.git` |
| `hetzner` | no remote (local-only) |
| `test1` | no remote (local-only) |
| `ferrisoft/company` | no remote (local-only) |

- `claude-status` contains about 35 Claude Code linked worktrees under `.claude/worktrees/`
  (their `.git` is a file); `ferrisoft/website` has one. Every repo with a remote has only
  `origin`. The history of all of them together is about 101 MB.
- The machines are the laptop, `demeter` and dev VMs (e.g. `dev-1`), all NixOS. Whether the
  dev VMs have cargo is unknown; the launcher prints a clear message when it's missing.

---

## 18. Open questions for the user (don't block on them)

1. GitHub names for the workspace repo (e.g. a private `wdanilo/dev2`) and for the tool repo
   (e.g. `wdanilo/dev_sync`). Needed for `init --tool-url` and `git remote add`.
   **Answered 2026-09-30:** the tool repo is a private `wdanilo/dev_sync`. The workspace repo's name is still open
   (a private `wdanilo/dev2` is the suggestion). Neither exists yet; creating them is outside §16. Since the redesign
   (§19) the tool repo is only where dev_sync is installed from, and `init` takes no tool URL; with teammates using
   it, its home is open again (item 7). **Settled 2026-09-30:** the user created `ferrisoft/dev_sync` and asked for
   the code to be pushed there (§19, "Published").
2. Filling `~/dev2`: `import ~/dev` + `pull` (fresh clones, about 100 MB), or moving the
   existing clones into `~/dev2` and running `push`.
   **Answered 2026-09-30:** not now; `~/dev2` stays uncreated.
3. Toolchain: built on stable because no nightly is installed. Confirm, or provide a nightly.
   **Answered 2026-09-30:** stay on stable.
4. Whether `~/dev2` later replaces `~/dev` (nothing hardcodes the path).
   **Answered 2026-09-30:** `~/dev` stays as it is for now. (Since the redesign, `dev_sync init ~/dev` would only
   add a hidden `.dev_sync` folder and leave the clones in place, so a separate `~/dev2` is optional.)
5. (From the review.) The network limits are wall-clock (300 s fetch/push, 1800 s clone), so a healthy but slow
   first clone of a big repository is stopped too; `DEV_SYNC_NETWORK_TIMEOUT_SECS` raises both. Switch to an
   inactivity limit (git's progress output restarting the clock)?
   **Answered 2026-09-30:** yes — "the more resilient way, that works even with a flaky network". Implemented as
   stall limits; see §19, "Network limits are stall limits".
6. (From the review.) With no state file — a new machine, or a lost `state.toml` — a clone of a repository the
   layout removed earlier is now treated as a blocked removal: the next pull moves it to the Trash if nothing in it
   exists only on that machine (`dev_sync keep` puts it back). Keep that, or never remove in this case and only
   report?
   **Answered 2026-09-30:** keep it — trash it when that's safe.
7. (Raised by the user, 2026-09-30.) Teammates are expected to use dev_sync too, each with their own workspace repo
   and all sharing the tool repo. Nothing in the code is tied to one person, but §2 scoped it to a single user, so:
   every teammate needs read access to the tool repo (a private repo under `wdanilo` means adding collaborators; the
   `ferrisoft` org is the natural home), push access to it lets code run on every teammate's machine at their next
   `pull` (self-update), and it runs on Linux only (§16's non-goals exclude macOS and Windows). Open: where the tool
   repo lives, and whether any teammate needs macOS. **Update 2026-09-30:** after the redesign (§19) there is no
   self-update: a push to the tool repo reaches a machine only when its owner reinstalls. Read access is still needed,
   to install. The tool repo is now `ferrisoft/dev_sync` (item 1); macOS is still open.

---

## 19. Deviations log

Record every change to this design made during implementation, with the reason.

**Installed tool, hidden workspace repository** (the user's decision, 2026-09-30, after the first implementation)

- **Why.** The user asked why the workspace carries a script that downloads and compiles the tool, instead of the
  tool being installed like any other program — and whether the workspace repository could be hidden instead of
  making the dev folder itself a git repository. Both changes suit a tool teammates will use too (§18.7): installing a
  program is the familiar step; a push to the tool repo no longer reaches every machine on its next pull; and a dev
  folder that is not a git repository keeps git, shell prompts and editors in its in-between folders (say
  `~/dev/ferrisoft`) from finding the workspace repository — the reason the user's dotfiles live in
  `~/.dotfiles.git` rather than `~/.git`.
- **Installed tool.** dev_sync is installed once per machine (`cargo install`; the README has the steps) and runs
  from `PATH`. Removed: the `./sync` launcher and its template, the `.setup` clone, the self-update (§8.11) and
  `DEV_SYNC_SELF_UPDATED`, `init --tool-url`, `.setup`'s content push and status, `report::Scope::Tool`. What is
  given up: machines no longer update together, so two can run different versions for a while. `repos.toml`'s
  `format` still guards the file: an older dev_sync refuses a newer format ("install a newer dev_sync").
- **Hidden workspace repository.** The workspace repository is `<root>/.dev_sync`, a normal git repository whose
  work tree holds `repos.toml` and `.gitattributes` (no `.gitignore` needed: nothing else lives there). The root holds
  only the clones and that hidden folder; the scan skips it like any hidden folder. Discovery walks up to the first
  folder whose `.dev_sync` holds `repos.toml` and `.git`. State, lock and the interrupted-commit marker stay in the
  repository's git dir (`<root>/.dev_sync/.git/dev_sync/`); move and clone temps stay where they were. Git commands
  for the workspace repository take a `workspace::Repository` — its own type — so none can run on the root or a
  clone by mistake. Hints name `.dev_sync` paths (`git -C <root>/.dev_sync …`, "resolve `<root>/.dev_sync/repos.toml`").
- **The merge driver** is always the running executable (`<exe> merge-driver %O %A %B %P`, no `--root`: the driver
  never needed it), re-registered by every changing command, so a reinstalled dev_sync at a new path takes over.
- **Workspaces never nest.** `init` refuses a folder inside another workspace, and a scan that meets another
  workspace below its root stops with an error: the clones there would belong to both. A `.dev_sync` at the root of a
  tree given to `import` is fine.
- **A new machine** clones the workspace repository into `<dir>/.dev_sync` and runs `dev_sync pull`; `init` on a
  folder that already holds clones leaves them in place, and the first push records them. So an existing folder like
  `~/dev` can become a workspace in place; a fresh `~/dev2` is no longer needed (the user decides when).
- **User-facing text** says `dev_sync <command>` instead of `./sync <command>`, and `repos.toml`'s header says
  "Written by dev_sync.".
- **Tests.** Scenarios 18 (self-update), 21b and 21c (the launcher) were replaced by 18 (the workspace is found from
  anywhere inside it; `--root` from outside) and 21b (workspaces never nest); scenario 01 checks the new layout;
  scenario 34's `.setup` half became "a pull still clones the rest"; scenario 40's second conflicting file is a new
  `notes` file, since `.gitignore` is gone. A real run installed dev_sync with `cargo install` into a scratch folder,
  turned a folder holding a clone into a workspace, pushed from inside a clone, and pulled on a second scratch
  machine with `--root` from outside.

**Process**

- **`install.sh`** (2026-09-30, at the user's request: the `cargo install` line was too complex). A POSIX sh script
  at the repository root: it checks for git and cargo, runs `cargo install --locked --force` — from its own clone
  when run as a file there, otherwise from `DEV_SYNC_REPOSITORY` (default the https URL) when piped into `sh` —
  then says whether `~/.cargo/bin` is on `PATH` and what to do next. The repository stays private for now ("people
  who have access will have access") and may become public later, so the README gives `git clone … && sh
  dev_sync/install.sh` today and the `curl … | sh` form for later; the script needs no change in between.
  `dev_sync --version` came with it. Tested run from a clone, piped with a local repository as the source, and with
  no cargo on `PATH`.
- **Published** (2026-09-30, at the user's request, which lifts §14's commit rule and §16's no-push rule for this):
  the worktree was left, the code committed on `main` in `~/dev/dev_sync`, and `main` pushed to the empty
  `git@github.com:ferrisoft/dev_sync.git`.
- **Where the work lives.** The implementing session ran as a background job that must isolate its edits in a git
  worktree, and the repo had no commit to branch from. The code was written in an orphan linked worktree,
  `.claude/worktrees/dev-sync-impl` (branch `dev-sync-impl`), uncommitted per §14's commit rule; the main checkout was
  left as found.
- **Three reviewers, not five** (§15), at the user's request; the five focus areas were folded into three.
- **TDD for the command layer.** The e2e scenarios were the failing tests for `workspace.rs`, `commands/*` and
  `main.rs`; their few unit tests were written alongside.
- **No rust-analyzer.** The session's language server was rooted at the main checkout, which has no `Cargo.toml`, so
  it saw nothing of the worktree; the user allowed continuing without it. `cargo check`, clippy and the tests gated
  every change.
- **Review triage.** Every finding was reproduced (or checked in the code) before acting; fixes went test-first —
  the new test failed on the old code — and tests written after existing behavior were checked by temporarily
  breaking the code they guard. The outcome of every finding is in the final report; the design changes are below,
  marked "after review".

**Dependencies**

- `trash` gets its `chrono` feature: without it, `.trashinfo` files have no `DeletionDate` (required by the
  freedesktop spec).
- `rustix` (features `process`, `std`; already in the tree through `tempfile`) sends signals to timed-out children
  through a safe API, since `unsafe_code` is forbidden (after review).

**Process runner and git (§10)**

- Reader threads stream chunks over one channel instead of sending a whole buffer at EOF, so output written before a
  grandchild (e.g. ssh) holds a pipe open survives the bounded wait. A child that exits on its own right at the
  deadline counts as finished.
- **Stopping at the deadline** (after review): instead of SIGKILL to git alone, the child and all its descendants
  (read from `/proc/<pid>/task/*/children`) get SIGTERM — git removes its lock files, ssh restores a terminal it
  prompts on — and the child gets SIGKILL after 3 s. Only the child is ever SIGKILLed, since only its pid can't be
  reused before it is reaped. Process groups were rejected: a child in its own group can't read the terminal, which
  breaks ssh prompts. A local command that timed out names an `index.lock` left in its repository.
- `RemoteFailure::detail` is the stderr line that explains the failure — the first line matching the winning
  classification rule, else the first `ERROR:`/`fatal:`/`error:` line, else the first line, leaving out git's
  boilerplate ("Could not read from remote repository.", "Please make sure you have the correct access rights…",
  "and the repository exists.") and `hint:` lines, without the prefix — instead of the last three lines, which for ssh
  failures are exactly that boilerplate. `RemoteFailure` also keeps `stdout`, so a partly rejected push still reports
  each ref.
- **Network limits are stall limits, not wall-clock** (after review; the user chose it — §18.5). §10 stopped a
  fetch or push after 300 s and a clone after 1800 s however well it was going, so a slow but healthy first clone
  of a big repository failed. Now a network command is stopped only after `stall_limit` (300 s;
  `DEV_SYNC_NETWORK_TIMEOUT_SECS`) without output: the runner inserts `--progress` after the subcommand, so git
  reports progress into a pipe too, and every byte on stdout or stderr restarts the clock (`process::Limit::Silence`).
  Clone runs without `--quiet`, which would silence its checkout phase. A stopped command is
  `RemoteFailureKind::Stalled { silence }` — "stalled — it made no progress for N s; check the connection, then run
  it again" — and isn't retried: another attempt would hang as long. Dead connections are caught sooner by the
  transports and fail as `Network`, which is retried: ssh keepalives now allow about 2 min
  (`ServerAliveInterval=30`, `ServerAliveCountMax=4`; §10's 15×3 can trip on a congested link or a laptop waking
  from sleep while the connection is still good) and curl's low-speed abort 120 s (`http.lowSpeedTime=120`, up from
  60). Local commands that may run long (layout commits, merges, fast-forwards) get the same silence limit; quick
  local commands keep a 60 s total limit. Replaces an earlier revision's wall-clock `Timeout { limit }`.
- `RemoteFailure::detail` reads each stderr line as a terminal shows it (after its last `\r`) and passes over
  progress reports, so a failure is explained by its error line and a stall by the last thing git printed.
- Classification: `does not exist` added to NotFound (git's message for a missing local-path remote); after review,
  `denied to` (GitHub's "Permission to … denied to …"), `saml sso` and `unable to get password from user` to Auth,
  and `operation too slow` (curl's low-speed abort) and `returned error: 5` (HTTP 5xx) to Network, after the Auth and
  NotFound rules.
- Branches are read with `%(refname)` rather than `%(refname:short)`, which can be ambiguous (`heads/main` when a tag
  `main` exists). Branches whose upstream is a local branch (remote `.`) are never pushed.
- `git worktree list --porcelain -z`, so worktree paths may contain newlines.
- `core.sshCommand` is checked where each network command runs (so a repository's own setting wins), not once at the
  root.
- Every git command on a repository runs as `git -C <dir> --git-dir=.git --work-tree=.` and loses
  repository-selecting variables (`GIT_DIR`, `GIT_WORK_TREE`, …): a directory that lost its `.git` fails instead of
  reaching the workspace repo around it, where a removal safety check would pass on the wrong repository. (First
  `GIT_CEILING_DIRECTORIES=<parent>`; after review, pinned instead, because the ceiling list can't hold a path with
  `:`.) `git clone` runs in its parent directory without pinning.
- Local commands that may wait for the user or run user code (layout commits and the workspace merge, e.g. a
  signing passphrase; fast-forwards of repos, e.g. LFS smudge filters and post-merge hooks) get the network stall
  limit instead of 60 s. The workspace merge passes `--ff` to override a user's `merge.ff=only`.
- Prompt-free (parallel) network operations also get `GIT_ASKPASS=` (empty: git skips `core.askPass` and
  `SSH_ASKPASS` too) and `-c credential.interactive=false`; `GIT_TERMINAL_PROMPT=0` alone still ran askpass programs
  and waited until the time limit (after review).
- Remote names are a validated type (`RemoteName`: never empty, `.`, or starting with `-`); an upstream on a remote
  named like an option is reported and never passed to git — the layout push had no such check (after review).

**State and lock (§6)**

- **The lock is an `flock`** (§6.3 described a `create_new` PID file removed on exit). Taking over a stale PID file
  was racy — two runs could both hold it — and a PID reused after a crash blocked every run until the file was
  deleted by hand. `File::try_lock` on `.git/dev_sync/lock` is released by the kernel however the process ends; the
  file stays and holds the last holder's PID, only for the "another dev_sync is running (pid N)" message.
- The state directory is fsynced after the rename.
- **An interrupted layout commit converges** (after review). `commit_layout` writes the text it commits to
  `.git/dev_sync/committing` first and removes it once `repos.toml` is committed (or restored after a failed
  commit). The next changing command finding it restores `repos.toml` from HEAD — only if the file still holds
  exactly that text, so a hand edit is never discarded — and reports it; the base wasn't saved, so the change is
  recorded again. Before, a Ctrl-C during a commit hook left `repos.toml` modified and every later run refused.
- **Lost state and blocked removals** (after review): §6.2 called a lost `state.toml` harmless, but it turned blocked
  removals back into additions. With no state file, a clone the current layout lacks but whose canonical entry
  (`git log -S` on `repos.toml`) the layout had before is held back as a blocked removal instead of being recorded.

**Layout sync (§8)**

- `scan` returns `Scanned { repos, leftovers }`: leftover `.dev_sync-*` directories are found during the same walk.
- `detect` matches by `.git` id for every clone before falling back to paths. Otherwise a fresh clone at a moved
  repo's old path takes over the base entry, and the moved repo drops out of the layout.
- `apply_to_snapshot`: a local removal or move only takes a path out of the snapshot when the snapshot has the same
  URL there. When it holds another repository (say, a conflict resolved in favor of the other machine), that
  repository isn't what left this machine: the removal is dropped and the move becomes an addition of its
  destination.
- **Interrupted moves are put back.** Before detecting changes, a repository parked in `.dev_sync-moving-*` whose
  `.git` id the base knows goes back to its base path. Only reporting it (as §8.8 said) would let the next record see
  it as removed and spread the removal to every machine. A parked repository whose old place is taken stops the
  command with instructions; one the base doesn't know is reported with its origin (never with a suggestion to delete
  it) and left alone.
- `reconcile/facts.rs` collects `DiskFacts`; obstacles are an enum, and a file or symlink where a parent directory
  should be counts as one. A directory holding nothing but empty directories and repositories is its own fact: it
  can take a repository once those repositories are gone (it used to count as a non-empty directory in the way).
- **Moves can't strand a repository** (after review; §8.8 moved in two phases without a feasibility check). The
  planner keeps only moves and clones whose destination can take a repository once the removals and the kept moves'
  sources are gone (`plan::landing`), dropping moves until none is blocked: a move onto a repository whose own move
  was dropped is dropped too. The executor repeats that check on fresh disk facts after the removals, since a
  blocked removal still occupies its place; lands the parked repositories from the free end of each chain, so a
  failed move finds its source free; and, within a cycle, lifts the moves that already landed on a failed move's
  source back into their parking spots before putting it back. Before, a chain whose later step failed left a
  repository in `.dev_sync-moving-*` that the next run didn't know. A randomized test drives the real planner and
  executor over 1500 random layouts.
- `pull --continue` runs the record step after committing the merge, so clones moved while resolving are recorded
  before the disk is reconciled.
- Removal safety adds `NotARepository` (a directory that lost its `.git`), `DetachedCommits` (separate from branch
  commits, which would otherwise be counted twice), and worktree problems `OperationInProgress` and `Inaccessible`.
- **Removal fetches first** (after review). §8.9 said stale remote-tracking refs only make the check more cautious;
  they don't: a branch deleted or force-pushed on the remote still "covers" commits that now exist only here. A
  removal runs `fetch --all --prune` and keeps the clone when that fails. Commits only a tag holds count too
  (`--branches --tags`).
- **Never copy into the Trash** (after review): when the Trash is on another filesystem than the clone, the `trash`
  crate copies the whole repository, then deletes it. The removal compares devices first and asks the user instead.
- **A url change checks history first** (after review). The planner still turns "same path, new URL" into a url
  change, but the executor first fetches the new URL's HEAD into `refs/dev_sync/probe` and only repoints the clone
  when that history is shared (or the clone has none); an unrelated repository at that URL is reported and the clone
  kept, a URL it can't reach is retried by the next pull.
- **Layout merge** (after review). A merge driver that can't run left git's "ours" in `repos.toml`, which `pull
  --continue` then committed, silently dropping the incoming changes. Now: the driver exits 255 on any error (git
  reads 1–128 as a conflict), and so does the launcher when it runs as the driver; `pull` merges with `--no-commit`
  and, whenever git stops, writes the in-process merge of the three layouts into `repos.toml`, so the result never
  depends on the driver or on git's text merge; a clean merge is only committed after it parses; `pull --continue`
  refuses a resolution that changes what merged cleanly (`layout::stray_changes`). The driver command escapes `%`
  in paths as `%%`.
- `clone --origin origin`, so a user's `clone.defaultRemoteName` can't leave clones without an `origin`.
- **Content push only to a same-named upstream** (after review; §8.10 pushed every branch ahead of its upstream). A
  branch created from `origin/main` tracks `main`, so the old rule pushed work in progress onto `main`, and two
  branches tracking one remote branch failed the whole push. A branch whose upstream has another name gets a note;
  branches without an upstream share one note per repository.
- **A fetch that reached the remote still counts** (after review): content fetches drop `--quiet` (git's reason was
  lost). When a fetch got through to the remote but failed at something smaller — a ref it rejected, such as a tag
  it won't clobber (attention), or an error it can't classify (failure) — the other refs were updated, so the
  repository is still inspected and fast-forwarded. Only network, stall, auth and not-found failures skip it.
- `Layout::apply` applies url changes after the removal phase and before insertions (§7 puts them in the insertion
  phase). The result is the same: a url change needs its path present, an addition needs it absent.

**CLI and output (§9)**

- **`list [DIR]`** (the user's request, 2026-10-01): prints `DIR`, or the workspace root, as a tree drawn like
  `tree`, going no deeper than a repository. Folders end in `/`; a folder with no repository anywhere inside — so
  nothing in it is synced — is one red line (with colors off: `(no repositories)`), its inside not listed. Hidden
  entries, files and symlinks are left out, as the scan leaves them out; a folder whose `.git` is a file is labelled
  a linked worktree (not synced); an unreadable folder shows the reason instead of stopping the listing. It reads
  only the disk (not the layout), needs no network, and with `DIR` no workspace, so it also shows a folder before
  `init`. The tree lives in `listing.rs`; `list` prints it directly rather than as a report.
- The result of pushing the layout is reported under the layout scope, so it prints after the `recorded …` lines.
  It is read from the porcelain stdout: a rejected push is recognized even with `advice.pushUpdateRejected=false`,
  and a ref the remote refused (a hook, a protected branch) carries the remote's reason.
- `status` prints `✓ nothing to do (as of the last fetch)` when it has nothing to report; `pull` prints `✓ already
  up to date`.
- The self-update re-exec passes `--verbose` along when it was given.
- `import` writes `import 1 repo` / `import N repos`, and refuses a directory inside or containing the workspace.
- Added modules: `commands/session.rs` (lock, preamble, shared record/reconcile steps), `parallel.rs`, `shell.rs`,
  `fixtures.rs` (tests only).
- **Exit codes** (after review): §9.10's "every error exits 1" would make git read a failed merge driver as a
  conflict; the driver uses its own `DriverOutcome` (0 clean, 1 conflicted) and 255 on errors.
- **`status` after review**: while a layout merge is in progress it gives only the merge's own advice (pulling and
  discarding `repos.toml` both fail then); a pending repository something on disk keeps out is reported with what's
  in the way instead of "`./sync pull` clones it"; errors in one blocked repository, in `.setup`, or in the scan are
  report items, and the rest is still shown.
- A `.setup` git can't work with no longer stops `pull`: the self-update is skipped with an attention line. Lines
  about the tool are prefixed `.setup:`.
- The launcher template: exits 255 when running as the merge driver fails, and names `git -C .setup reset --hard
  ORIG_HEAD` when building an updated tool fails (after review). The launcher was removed later (the redesign above),
  and so was the `.setup:` prefix of the line before.

**Tests (§13)**

- Unit tests run git through a runner with its own environment (`Git::with_environment`, test-only): no system or
  global config and no default excludes file, a fixed identity. §13.3 asked for isolation, but code under test used
  to inherit the developer's configuration (the laptop's system config sets identity and `init.defaultBranch`; a
  global `core.excludesFile` or `clone.defaultRemoteName` made two tests fail). The e2e `isolate` also strips
  `GIT_INDEX_FILE`, `GIT_CONFIG_PARAMETERS`, askpass variables and the like, and sets `XDG_CONFIG_HOME`.
- Scenarios beyond §13.2: 21b/21c (the launcher; since replaced, see the redesign above), 26–41 (from the review:
  driver failure, closed stderr, text-merge validity, rejected layout push, removal against the remote as it is now,
  url change to an unrelated repo, moves held back by a blocked removal, an interrupted layout commit, broken
  repositories, status during a merge, pending clones that can't land, quiet runs, lost state with a blocked removal,
  command edges, `pull --continue` with other conflicts, a remote named like an option).

**Environment findings**

- NixOS's `cc` wrapper can't link when `CARGO_TARGET_DIR` contains `'` or non-ASCII. The launcher's
  `${XDG_CACHE_HOME:-$HOME/.cache}/dev_sync/target` was unaffected (the launcher is gone since the redesign; the
  finding still holds for anyone pointing `CARGO_TARGET_DIR` at such a path).
- The launcher's first build (before the redesign), measured from a clean target directory on the laptop (16 cores,
  dependencies already downloaded): 19 s, about 300 MB in `~/.cache/dev_sync/target` (the debug binary alone 53 MB).
  After the redesign, `cargo install` builds in release mode in a temporary directory it deletes afterwards: 26 s,
  about 109 MB while building, and a 4.2 MB binary left in `~/.cargo/bin`. The Linux dependencies are 71 crates,
  about 11.5 MiB, downloaded into cargo's cache (`~/.cargo/registry`).
- `status` on a scratch workspace from `init` + `import ~/dev` (8 repos, none cloned) took 0.02–0.03 s in release
  builds (5 runs, ~5 MB RSS). The optional real-network `pull` was skipped: from a background job it would have used
  the desktop's keyring ssh agent, which can show an unlock prompt.
