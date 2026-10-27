#!/usr/bin/env bash
# Fails if a product package's dependency tree, for every target platform,
# contains anything other than workspace crates (path crates inside this
# workspace, whatever their name) and that package's allow-listed crates
# (NFR-MAINT-04):
#   durchlauferhitzer-core  nothing else
#   durchlauferhitzer       libc only (the driver; CLAUDE.md "Rust only")
# PRODUCT_PACKAGE=<name> checks only that package (with its allow-list).
#
# `check-product-deps.sh --filter ROOT ALLOWED` runs only the check: it
# reads `cargo tree --prefix none --format '{p}'` lines on stdin, prints
# each disallowed name and exits 1 if there is one (ROOT is the workspace
# root, ALLOWED a space-separated allow-list). scripts/test-check-product-deps.sh
# tests the check this way.
set -euo pipefail

# filter ROOT ALLOWED: the check itself, on stdin; prints disallowed names.
filter() {
    local root=$1 allowed=$2 name source a ok status=0
    # Each line is `name vX.Y.Z [(source)] [(*)]`.
    while read -r name _ source _; do
        [[ -z "$name" ]] && continue
        # Workspace crates (the package itself among them): a path source
        # below the root, whatever the name.
        [[ "$source" == "($root/"* ]] && continue
        ok=no
        for a in $allowed; do
            [[ "$name" == "$a" ]] && ok=yes
        done
        if [[ "$ok" != yes ]]; then
            echo "$name"
            status=1
        fi
    done
    return "$status"
}

if [[ "${1:-}" == --filter ]]; then
    [[ $# -eq 3 ]] || {
        echo "usage: $0 --filter ROOT ALLOWED" >&2
        exit 2
    }
    filter "$2" "$3"
    exit $?
fi

allowed_for() {
    case "$1" in
        durchlauferhitzer-core) echo "" ;;
        durchlauferhitzer) echo "libc" ;;
        *)
            echo "unknown product package: $1" >&2
            exit 2
            ;;
    esac
}

# The workspace root: a crate is a workspace crate when its source is a
# path below it (registry and git crates have no path source).
root=$(dirname "$(cargo locate-project --workspace --message-format plain)")

check() {
    local package=$1 allowed tree bad
    # `check` runs under `||`, where `set -e` is off: every failure returns
    # explicitly. The tree is captured first because a failure inside
    # process substitution would go unseen.
    allowed=$(allowed_for "$package") || return 1
    tree=$(cargo tree --locked -p "$package" -e normal,build --target all \
        --prefix none --format '{p}') || return 1
    if ! bad=$(filter "$root" "$allowed" <<<"$tree"); then
        echo "Disallowed packages in the dependency tree of $package:" >&2
        sed 's/^/  /' <<<"$bad" >&2
        return 1
    fi
}

status=0
for package in ${PRODUCT_PACKAGE:-durchlauferhitzer-core durchlauferhitzer}; do
    check "$package" || status=1
done
exit "$status"
