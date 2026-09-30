#!/bin/sh
# Installs or updates dev_sync (https://github.com/ferrisoft/dev_sync) with cargo, into ~/.cargo/bin.
#
#   From a clone:     sh dev_sync/install.sh
#   Without a clone:  curl -fsSL https://raw.githubusercontent.com/ferrisoft/dev_sync/main/install.sh | sh
#                     (once the repository is public)
#
# DEV_SYNC_REPOSITORY overrides where the second form fetches the code from.
set -eu

repository="${DEV_SYNC_REPOSITORY:-https://github.com/ferrisoft/dev_sync.git}"

say() {
    printf 'dev_sync: %s\n' "$*"
}

fail() {
    printf 'dev_sync: %s\n' "$*" >&2
    exit 1
}

command -v git >/dev/null 2>&1 || fail "git is missing; install it, then run this again"
command -v cargo >/dev/null 2>&1 ||
    fail "Rust is missing; install it (https://rustup.rs, or cargo from your distribution), then run this again"

# Run as a file inside a clone: build that clone. Piped into sh: fetch the code with git.
checkout=""
if [ -f "$0" ]; then
    here=$(cd -- "$(dirname -- "$0")" && pwd)
    if grep -qx 'name = "dev_sync"' "$here/Cargo.toml" 2>/dev/null; then
        checkout=$here
    fi
fi

if [ -n "$checkout" ]; then
    say "building $checkout (this takes a minute)"
    cargo install --quiet --locked --force --path "$checkout"
else
    say "building $repository (this takes a minute)"
    CARGO_NET_GIT_FETCH_WITH_CLI=true cargo install --quiet --locked --force --git "$repository" dev_sync
fi

bin="${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}/bin"
say "installed $("$bin/dev_sync" --version) into $bin"
case ":$PATH:" in
    *":$bin:"*) ;;
    *)
        say "$bin is not on your PATH yet; add this line to your shell's startup file, then open a new shell:"
        printf '  export PATH="%s:$PATH"\n' "$bin"
        ;;
esac
say "next: on your first machine, 'dev_sync init ~/dev'; on the others,"
say "      'git clone <your workspace repo> ~/dev/.dev_sync && cd ~/dev && dev_sync pull'"
