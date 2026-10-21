#!/usr/bin/env bash
# Fails if the product's dependency tree contains anything other than
# workspace crates (durchlauferhitzer-*) and the allow-listed crates.
set -euo pipefail

ALLOWED=(libc)

# Capture first so a failing `cargo tree` aborts the script (set -e does not
# see failures inside process substitution).
tree=$(cargo tree -p "${PRODUCT_PACKAGE:-durchlauferhitzer-core}" -e normal,build --prefix none --format '{p}')

bad=()
while read -r name _; do
    [[ -z "$name" ]] && continue
    [[ "$name" == durchlauferhitzer-* ]] && continue
    allowed=no
    for a in "${ALLOWED[@]}"; do
        [[ "$name" == "$a" ]] && allowed=yes
    done
    [[ "$allowed" == yes ]] || bad+=("$name")
done <<<"$tree"

if ((${#bad[@]} > 0)); then
    echo "Disallowed packages in the product dependency tree:" >&2
    printf '  %s\n' "${bad[@]}" >&2
    exit 1
fi
