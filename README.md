# dev_sync

Keeps your dev folder — a tree of independent git clones — the same on every machine you use.

## How it works

- Your dev folder (say `~/dev`) holds your clones and one hidden folder, `.dev_sync`: a small git repository with
  `repos.toml`, the list of which repository lives at which path. You give it a private remote, and every machine
  pulls and pushes it.
- `dev_sync push` records what changed on disk (clones you added, moved with `mv`, or deleted), pushes your repos'
  branches that are ahead of their upstream, and publishes the list.
- `dev_sync pull` merges changes from your other machines and makes the disk match: it clones new repositories, moves
  moved ones, moves removed ones to the Trash (only when nothing in them exists solely on this machine), and
  fast-forwards each repository.
- It never force-pushes, merges or rebases your work. A real conflict stops the pull and is left to you.

## Requirements

Linux, git, and Rust 1.95 or newer to install it (`rustup`'s stable toolchain is fine). Your usual ssh keys or
credential helper for the repositories in your list — dev_sync runs your `git`.

## Install

```sh
cargo install --locked --git ssh://git@github.com/ferrisoft/dev_sync.git
```

It builds dev_sync (about a minute) and installs it into `~/.cargo/bin`; cargo tells you if that folder isn't on your
`PATH` yet. It fetches the repository over ssh, so you need an ssh key with access to it; once the repository is
public, `https://github.com/ferrisoft/dev_sync` works without one.

Add `--force` to update; `cargo uninstall dev_sync` removes it.

## Set up

On the first machine:

```sh
dev_sync init ~/dev                                              # adds ~/dev/.dev_sync; your clones stay put
git -C ~/dev/.dev_sync remote add origin git@github.com:<you>/dev.git    # an empty private repository
cd ~/dev && dev_sync push                                        # records your clones and publishes the list
```

On every other machine:

```sh
git clone git@github.com:<you>/dev.git ~/dev/.dev_sync
cd ~/dev && dev_sync pull                                        # clones everything
```

## Daily use

From anywhere inside the dev folder (or from anywhere with `--root ~/dev`):

```sh
dev_sync pull      # before you start working
dev_sync push      # before you switch machines
dev_sync status    # what push and pull would do, and what exists only here; no network
```

- Clone, move and delete repositories with your usual tools; the next push records it.
- A clone inside a hidden folder (such as `~/dev/.scratch/x`) is never synced, and one without an `origin` remote is
  only reported.
- When a pull stops on a conflict, edit `~/dev/.dev_sync/repos.toml` as it says, then run `dev_sync pull --continue`
  (or `dev_sync pull --abort`).
- When a removal is blocked because the clone holds work that exists only on this machine, push or discard that work
  and pull again, or run `dev_sync keep <path>` to keep the repository in the list.
- Everyone has their own workspace repository. Don't share one between people: whatever one of them adds, moves or
  removes would happen on everyone's disk.

The design and its rationale are in [`docs/design.md`](docs/design.md).
