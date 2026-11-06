#!/bin/sh
# Finds the commit that changed system calls per request: a `git bisect
# run` script. Usage (from the repository root):
#
#   bench/bisect.sh <scenario> <max-syscalls>
#
# Builds the release binary of the current checkout, runs the bench tool
# for that one scenario (one run, the strace pass only) and compares
# syscalls_per_req with the threshold.
#
# Exit status: 0 good (at most <max-syscalls>), 1 bad (above it), 125
# cannot judge (build failed, bench failed or reported errors: `git
# bisect` skips the commit), 2 bad usage (stops the bisect).
#
# Environment:
#   BENCH_DIR       the bench crate (default: bench in the current tree).
#                   Point it at a fixed checkout when the bisected range
#                   starts before the bench tool existed.
#   BENCH_DURATION  seconds per window (default 2).
#
# Copy the script out of the tree first, so that it survives checkouts:
#   cp bench/bisect.sh /tmp/bisect.sh
#   git bisect start <bad> <good>
#   git bisect run /tmp/bisect.sh static-small 25
# (The bench tool is built from BENCH_DIR, not from the bisected tree.)

if [ "$#" -ne 2 ]; then
    echo "usage: bisect.sh <scenario> <max-syscalls>" >&2
    exit 2
fi
scenario=$1
max=$2
case $max in
'' | *[!0-9.]* | *.*.* | .)
    echo "bisect.sh: <max-syscalls> must be a number, got '$max'" >&2
    exit 2
    ;;
esac
bench_dir=${BENCH_DIR:-bench}
duration=${BENCH_DURATION:-2}

tmp=$(mktemp -d "${TMPDIR:-/tmp}/bisect.XXXXXX") || exit 125
trap 'rm -rf "$tmp"' EXIT
trap 'exit 130' HUP INT TERM

if ! cargo build --release --locked -p durchlauferhitzer; then
    echo "bisect.sh: build failed, skipping this commit" >&2
    exit 125
fi

if ! cargo run --release --locked --manifest-path "$bench_dir/Cargo.toml" -- \
    --binary "${CARGO_TARGET_DIR:-target}/release/durchlauferhitzer" \
    --out "$tmp/bench.csv" --runs 1 --duration "$duration" \
    --scenarios "$scenario" --syscalls-only; then
    echo "bisect.sh: bench failed, cannot judge this commit" >&2
    exit 125
fi

# One data row: errors and requests must be sane, syscalls_per_req set.
verdict=$(awk -F, -v max="$max" '
    NR == 1 { for (i = 1; i <= NF; i++) col[$i] = i; next }
    NR == 2 {
        e = $col["errors"]; r = $col["requests"]; s = $col["syscalls_per_req"]
        if (e != "0" || r + 0 == 0 || s == "") { print "unjudgeable"; exit }
        print (s + 0 <= max + 0) ? "good " s : "bad " s
    }
' "$tmp/bench.csv" 2>/dev/null)

case $verdict in
good*)
    echo "bisect.sh: good: $scenario ${verdict#good } <= $max syscalls per request"
    exit 0
    ;;
bad*)
    echo "bisect.sh: bad: $scenario ${verdict#bad } > $max syscalls per request"
    exit 1
    ;;
*)
    echo "bisect.sh: bench row has errors or no numbers, cannot judge" >&2
    exit 125
    ;;
esac
