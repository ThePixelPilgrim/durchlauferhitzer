#!/usr/bin/env bash
# Fails if a product package's dependency tree contains anything other than
# workspace crates (durchlauferhitzer-*) and that package's allow-listed
# crates (NFR-MAINT-04):
#   durchlauferhitzer-core  nothing else
#   durchlauferhitzer       libc only (the driver; CLAUDE.md "Rust only")
# PRODUCT_PACKAGE=<name> checks only that package (with its allow-list).
set -euo pipefail

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

check() {
    local package=$1 allowed tree name a ok
    # `check` runs under `||`, where `set -e` is off: every failure returns
    # explicitly. The tree is captured first because a failure inside
    # process substitution would go unseen.
    allowed=$(allowed_for "$package") || return 1
    tree=$(cargo tree --locked -p "$package" -e normal,build --prefix none --format '{p}') || return 1
    local bad=()
    while read -r name _; do
        [[ -z "$name" ]] && continue
        # The package itself and the workspace crates.
        [[ "$name" == "$package" || "$name" == durchlauferhitzer-* ]] && continue
        ok=no
        for a in $allowed; do
            [[ "$name" == "$a" ]] && ok=yes
        done
        [[ "$ok" == yes ]] || bad+=("$name")
    done <<<"$tree"
    if ((${#bad[@]} > 0)); then
        echo "Disallowed packages in the dependency tree of $package:" >&2
        printf '  %s\n' "${bad[@]}" >&2
        return 1
    fi
}

status=0
for package in ${PRODUCT_PACKAGE:-durchlauferhitzer-core durchlauferhitzer}; do
    check "$package" || status=1
done
exit "$status"
