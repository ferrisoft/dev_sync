# dev_sync workspace

This repository is a [dev_sync](https://github.com/ferrisoft/dev_sync) workspace: it keeps a dev folder — a tree of
independent git clones — the same on every machine. It lives in the dev folder's hidden `.dev_sync` folder; the clones
live next to it, in the dev folder itself.

| File | What it holds |
|---|---|
| `repos.toml` | Every repository in the dev folder: its path and its `origin` URL. dev_sync writes it; edit it by hand only to resolve a merge conflict. |
| `.gitattributes` | Tells git to merge `repos.toml` with dev_sync. |
| `README.md` | This note, written by `dev_sync init`. |

## Use it on another machine

Install dev_sync (`cargo install --locked --git ssh://git@github.com/ferrisoft/dev_sync.git`), then:

```sh
dev_sync init ~/dev --remote __REMOTE__
```

It clones every repository listed in `repos.toml` into `~/dev`. After that, run `dev_sync pull` before you start working
and `dev_sync push` before you switch machines.
