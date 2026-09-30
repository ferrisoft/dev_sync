# dev_sync

Keeps the user's dev folder (a tree of independent git clones, the "workspace") identical across machines. The
layout (repo path → origin URL, in `repos.toml`) lives in the workspace repository, a small git repo hidden in the
dev folder's `.dev_sync`; the dev folder itself is not a repository. dev_sync is installed per machine and run from
`PATH`: `dev_sync pull` / `dev_sync push` sync the layout and each repo's commits. The full design and rationale live
in [`docs/design.md`](docs/design.md) — read it before changing behavior, and record every deviation in its §19
(the launcher/self-update design of the first implementation was replaced; §19 "Installed tool, hidden workspace
repository"). [`README.md`](README.md) is the user guide. The repository is `git@github.com:ferrisoft/dev_sync.git`;
teammates install from it.

## Commands

```sh
cargo build
cargo test                                  # ~240 unit tests + 44 end-to-end scenarios (real git, ~7 s)
cargo clippy --all-targets -- -D warnings   # must be clean; never #[allow] in production code
cargo run -- --root <workspace> status      # during development
cargo install --locked --path .             # install this checkout as `dev_sync` (release build, ~30 s)
```

Stable Rust 1.95 (nixpkgs; no rustup, no nightly), edition 2024. No `rustfmt.toml`, so don't run rustfmt; keep lines
within 120 characters by hand. `unsafe_code` is forbidden; `unwrap`/`expect`/`panic`/indexing/slicing/`exit` are
denied by clippy (see `Cargo.toml`). Signals go through `rustix` (safe API), never `libc`.

## Module map

| Module | What it does |
|---|---|
| `main.rs` | parses the CLI, sets up logging and `NetworkPolicy`, dispatches, prints the report, picks the exit code |
| `cli.rs` | clap types |
| `commands/` | one file per subcommand; `session.rs` holds the lock + preamble + the shared record/reconcile steps |
| `domain/` | validated newtypes: `RepoPath`, `RemoteUrl`, `FileId`, `HostName`, `BranchName`, `CommitId`, `RemoteName` |
| `layout/` | `Layout` (+ batch `apply`), `Change` + `diff`, three-way `merge` (+ `stray_changes`), `repos.toml` parse/render |
| `process.rs` | child processes with a `Limit` (total time, or silence: no output for a while) and drained pipes; a stopped child gets SIGTERM with its descendants, SIGKILL after a grace period (knows nothing about git) |
| `git/` | `runner.rs` (env, pinned repositories, limits, retries), `failure.rs` (classify stderr), `parse.rs` (porcelain), `repo.rs` (queries, history probe) |
| `scan.rs` | walks the workspace for clones (never into repos or hidden dirs) and leftover temp dirs; another workspace below the root is an error |
| `state.rs` | per-machine base (`.dev_sync/.git/dev_sync/state.toml`, atomic) and the lock (`flock`) |
| `record.rs` | `detect` local changes, `apply_to_snapshot`, `next_base`, commit messages |
| `safety.rs` | `removal_safety`: every reason a clone can't go to the Trash |
| `reconcile/` | `facts.rs` (disk facts), `plan.rs` (pure planner + the `landing` check), `execute.rs` (trash/move/set-url/clone, restore moves) |
| `content.rs` | fetch / fast-forward / push of each repo's commits (pure decisions + execution) |
| `workspace.rs` | `Workspace` (root) and `Repository` (the hidden `.dev_sync` repo), discovery, `init`, merge-driver registration, workspace git queries, atomic layout commit (+ recovery of an interrupted one) |
| `listing.rs` | the tree `list` prints: a walk that stops at repositories and collapses folders without one, and its `tree`-style rendering |
| `report.rs` | `Report`/`Item`/`Severity`/`Scope`, rendering, exit code |
| `parallel.rs` | bounded, order-preserving `map` on scoped threads |
| `shell.rs` | sh quoting for the driver command and hints |
| `fixtures.rs` | test-only helpers (isolated git runner, sandboxed repos, test servers) |

## Environment variables

`DEV_SYNC_HOST` (host name in commit messages; tests), `DEV_SYNC_LOG` (tracing filter, overrides `--verbose`),
`DEV_SYNC_NETWORK_TIMEOUT_SECS` (the stall limit: how long a fetch/push/clone may show no progress before it is
stopped, default 300) / `DEV_SYNC_RETRY_BASE_DELAY_MS` (tests; unparsable = startup error).

## Tests

- Unit tests sit at the bottom of each module. Code under test runs git through `fixtures::git()`, which carries its
  own environment (`Git::with_environment`, test-only): no system or global config, no default excludes file, a fixed
  identity and `init.defaultBranch=main`. `fixtures::Sandbox` sets up repositories with its own isolated config.
  (The laptop's system config sets identity, `init.defaultBranch`, delta as pager, `safe.directory`; unit tests used
  to pick it up.)
- `tests/e2e.rs` runs the real binary: `tests/e2e/support.rs` builds a `World` of bare remotes and `Machine`s, each
  with its own `HOME`, `XDG_CONFIG_HOME`, `GIT_CONFIG_GLOBAL`, `XDG_DATA_HOME` (so the Trash is private) and a
  short stall limit; `isolate` strips every variable that would change git or dev_sync. Test servers bind
  `127.0.0.1:0` and stop on drop.
- Randomized checks use a fixed seed (`moves_never_strand_or_lose_a_repository` drives the real planner and executor
  over 1500 random layouts on disk).
- Run tests with `TMPDIR` pointing at a job-private directory when parallel sessions share `/tmp`.

## Gotchas

- The workspace root is not a git repository; the workspace repository is `<root>/.dev_sync`. Workspace git
  commands take a `workspace::Repository` (`repository.dir()` for `Git::at`), never the root — the separate type
  exists so the compiler catches a git call aimed at the wrong folder. The scan skips `.dev_sync` like any hidden
  folder, and stops with an error at another workspace below its root.
- `Git::at(dir)` runs `git -C <dir> --git-dir=.git --work-tree=.` with `GIT_DIR`/`GIT_WORK_TREE`/… stripped: the
  repository at `dir` and nothing else, so a directory that lost its `.git` fails ("not a git repository") instead of
  reaching the workspace repo around it. (`GIT_CEILING_DIRECTORIES` can't express a path with `:`.) `git clone` runs
  through `Git::in_directory`, never pinned: clone would take `GIT_DIR` for the new repository's git dir. Note that
  `git config --get` against a missing `.git` exits 1 (key not found), not 128.
- Prompt-free (parallel) network operations get `GIT_ASKPASS=` (empty skips every askpass program) and
  `-c credential.interactive=false`, besides ssh `BatchMode=yes`.
- Network operations are stopped only when they show no progress for the stall limit, never for taking long: the
  runner inserts `--progress` after the subcommand and every byte on stdout or stderr counts as progress. So never
  make a network command `--quiet` in a way that silences it (clone runs without `--quiet`: with it, the checkout
  phase prints nothing). Dead connections are caught sooner by ssh keepalives and curl's low-speed limit, which fail
  as `Network` and are retried; a stall is not retried.
- `git push --porcelain -u` also prints a "set up to track" line on stdout; the push parser skips non-ref lines.
  Push results are always read from the porcelain stdout, never from stderr hints (advice can be switched off).
- `git worktree list` is read with `-z`; porcelain v2 rename records carry an extra NUL-terminated original path;
  `status.showStash` adds a `# stash N` header entry.
- The lock is an `flock` on `.dev_sync/.git/dev_sync/lock`, which stays on disk. A child process another thread is
  starting shares the lock's file description until it execs, so a lock dropped and retaken at once in tests can look
  held for a moment (`state.rs` tests retry briefly); in production the lock lives as long as the process.
- `commit_layout` keeps the text it is committing in `.dev_sync/.git/dev_sync/committing` until the commit is done;
  the next changing command restores `repos.toml` from HEAD if it still holds exactly that text.
- The merge driver registered in `.dev_sync/.git/config` names the running executable's path; every changing command
  re-registers it, and `pull` also passes it with `-c`, so a reinstall at another path is picked up.
- `.dev_sync-moving-*` directories hold real repositories after an interrupted move: they're put back by the next
  pull/push when the base knows them, otherwise only reported (with their origin). Never delete them in code.
- Moves are checked against the disk after the removals (`plan::landing`) and land from the free end of each chain;
  a failure inside a cycle lifts the moves that already landed on its source back before putting it back.
- A url change is only applied when the new URL shares history with the clone (a fetch into `refs/dev_sync/probe`,
  deleted afterwards); a removal fetches first so stale remote-tracking refs can't make local work look pushed.
- NixOS's `cc` wrapper can't link when `CARGO_TARGET_DIR` contains `'` or non-ASCII, so never point a build at such
  a path (tests that build something must keep their target directory on a plain path).
- User-facing text names commands as `dev_sync <command>`, never `./sync …` (the launcher is gone).
- Users install with `cargo install --locked --git ssh://git@github.com/ferrisoft/dev_sync.git` (README). When
  trying an install, set `CARGO_INSTALL_ROOT` to a scratch folder: without it, the run installs into the real
  `~/.cargo/bin`.
- The `trash` crate needs its `chrono` feature, or `.trashinfo` files lack `DeletionDate`. It copies across
  filesystems, so removals check the device of the Trash first and never let it copy.
