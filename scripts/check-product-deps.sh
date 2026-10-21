#!/usr/bin/env bash
# Fails if the product's dependency tree contains anything other than
# workspace crates (durchlauferhitzer-*) and the allow-listed crates.
set -euo pipefail

ALLOWED=(libc)

bad=()
while read -r name _; do
    [[ -z "$name" ]] && continue
    [[ "$name" == durchlauferhitzer-* ]] && continue
    allowed=no
    for a in "${ALLOWED[@]}"; do
        [[ "$name" == "$a" ]] && allowed=yes
    done
    [[ "$allowed" == yes ]] || bad+=("$name")
done < <(cargo tree -p durchlauferhitzer-core -e normal,build --prefix none --format '{p}')

if ((${#bad[@]} > 0)); then
    echo "Disallowed packages in the product dependency tree:" >&2
    printf '  %s\n' "${bad[@]}" >&2
    exit 1
fi
