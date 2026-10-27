#!/usr/bin/env bash
# Self-test of scripts/check-product-deps.sh (NFR-MAINT-04). It feeds
# canned `cargo tree --prefix none --format '{p}'` lines, for a workspace
# rooted at /ws, to the script's `--filter` mode and checks which trees
# pass. Offline and deterministic: no cargo call.
set -euo pipefail

script="$(dirname "$0")/check-product-deps.sh"
failures=0

# expect <pass|NAME> <what> <allowed> — reads the tree on stdin. `pass`
# wants exit 0 and no output; NAME wants exit 1 with NAME reported.
expect() {
    local want=$1 what=$2 allowed=$3 out status
    out=$("$script" --filter /ws "$allowed") && status=0 || status=$?
    if [[ "$want" == pass ]]; then
        if [[ "$status" -ne 0 || -n "$out" ]]; then
            echo "FAIL: $what: expected pass, got exit $status: $out" >&2
            failures=$((failures + 1))
        fi
    elif [[ "$status" -ne 1 ]] || ! grep -qxF "$want" <<<"$out"; then
        echo "FAIL: $what: expected $want flagged (exit 1), got exit $status: $out" >&2
        failures=$((failures + 1))
    fi
}

expect pass "workspace crates and libc" libc <<'TREE'
durchlauferhitzer v0.1.0 (/ws/driver)
durchlauferhitzer-core v0.1.0 (/ws/core)
libc v0.2.191
TREE

expect pass "repeated entries marked (*)" libc <<'TREE'
durchlauferhitzer v0.1.0 (/ws/driver)
durchlauferhitzer-core v0.1.0 (/ws/core) (*)
libc v0.2.191 (*)
TREE

expect durchlauferhitzer-evil "a registry crate with the workspace prefix" libc <<'TREE'
durchlauferhitzer v0.1.0 (/ws/driver)
durchlauferhitzer-evil v1.0.0
TREE

expect evil "a path crate outside the workspace" libc <<'TREE'
durchlauferhitzer v0.1.0 (/ws/driver)
evil v0.1.0 (/elsewhere/evil)
TREE

expect evil "a path crate in a sibling directory sharing the root's prefix" libc <<'TREE'
durchlauferhitzer v0.1.0 (/ws/driver)
evil v0.1.0 (/ws2/evil)
TREE

expect serde "a registry crate that is not allow-listed" libc <<'TREE'
durchlauferhitzer v0.1.0 (/ws/driver)
serde v1.0.228
libc v0.2.191
TREE

expect libc "libc where nothing is allowed (the core)" "" <<'TREE'
durchlauferhitzer-core v0.1.0 (/ws/core)
libc v0.2.191
TREE

expect libc "an allow-listed name from a path outside the workspace" libc <<'TREE'
durchlauferhitzer v0.1.0 (/ws/driver)
libc v0.2.191 (/elsewhere/libc)
TREE

expect libc "an allow-listed name from git" libc <<'TREE'
durchlauferhitzer v0.1.0 (/ws/driver)
libc v0.2.191 (https://github.com/rust-lang/libc#0123abcd)
TREE

if ((failures > 0)); then
    echo "check-product-deps.sh self-test: $failures case(s) failed" >&2
    exit 1
fi
echo "check-product-deps.sh self-test: all cases passed"
