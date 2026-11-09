#!/usr/bin/env bash
# Self-test of scripts/cleanup-merged-branches.sh. Builds a throwaway
# "origin" (bare) and a clone in a temp dir, with a branch fast-forwarded
# into main, one rebased into main, an unmerged one, a protected data
# branch, one checked out in a second worktree, a merged remote-only one
# and a merged local-only one; then runs the script as dry run and with
# --delete. Offline (file remote); the temp dir is removed on exit.
set -euo pipefail

script="$(cd "$(dirname "$0")" && pwd)/cleanup-merged-branches.sh"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
failures=0

# A hermetic git: no user or system config, fixed identity.
export HOME="$tmp/home" GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
export GIT_AUTHOR_NAME=t GIT_AUTHOR_EMAIL=t@example.invalid
export GIT_COMMITTER_NAME=t GIT_COMMITTER_EMAIL=t@example.invalid
mkdir -p "$HOME"

fail() {
    echo "FAIL: $*" >&2
    failures=$((failures + 1))
}

# commit <file> <content> <message>
commit() {
    echo "$2" >"$1"
    git add "$1"
    git commit -qm "$3"
}

# has <what> <extended regex> — the captured $out must match.
has() {
    grep -Eq -- "$2" <<<"$out" || fail "$1: no line matching /$2/ in:"$'\n'"$out"
}
hasnt() {
    ! grep -Eq -- "$2" <<<"$out" || fail "$1: unexpected line /$2/ in:"$'\n'"$out"
}

# run <args...> — sets $out (stdout+stderr) and $status.
wd=$tmp/work
run() {
    out=$(cd "$wd" && "$script" "$@" 2>&1) && status=0 || status=$?
}

local_heads() { git -C "$tmp/work" for-each-ref --format='%(refname:short)' refs/heads | sort | paste -sd' '; }
remote_heads() { git -C "$tmp/origin.git" for-each-ref --format='%(refname:short)' refs/heads | sort | paste -sd' '; }

git init -q --bare -b main "$tmp/origin.git"
git clone -q "$tmp/origin.git" "$tmp/work" 2>/dev/null
cd "$tmp/work"
git checkout -q -b main 2>/dev/null || true
commit base.txt 0 base
git push -q origin main

git checkout -q -b ff main
commit ff.txt 1 "ff"
git push -q origin ff

git checkout -q -b remoteonly ff
commit ro.txt 1 "remote only"
git push -q origin remoteonly

git checkout -q -b rebased main
commit r.txt 1 "rebased"
git push -q origin rebased

git checkout -q -b unmerged main
commit u1.txt 1 "unmerged 1"
commit u2.txt 2 "unmerged 2"
git push -q origin unmerged

git checkout -q -b benchmarks main
commit data.txt 1 "bench data"
git push -q origin benchmarks

git checkout -q -b wt main
git push -q origin wt

git branch localonly main

git checkout -q main
git merge -q --ff-only remoteonly # brings in ff and remoteonly
commit m.txt 1 "main moves on"
git cherry-pick rebased >/dev/null # same patch as rebased, new SHA
git push -q origin main
git branch -q -D remoteonly
git worktree add -q "$tmp/wt-dir" wt

before_local=$(local_heads)
before_remote=$(remote_heads)

# --- usage ---
run --help
[[ $status -eq 0 ]] || fail "--help exit $status"
has "--help" '^usage:'
run --bogus
[[ $status -eq 2 ]] || fail "unknown option exit $status (want 2)"

# --- dry run ---
run
[[ $status -eq 0 ]] || fail "dry run exit $status: $out"
[[ $(local_heads) == "$before_local" ]] || fail "dry run changed local branches"
[[ $(remote_heads) == "$before_remote" ]] || fail "dry run changed remote branches"
has "dry run ff local" '^would delete +local +ff +\(merged: ancestor of origin/main\)'
has "dry run ff remote" '^would delete +remote +ff +\(merged: ancestor of origin/main\)'
has "dry run rebased local" '^would delete +local +rebased +\(merged: patch-equivalent'
has "dry run rebased remote" '^would delete +remote +rebased +\(merged: patch-equivalent'
has "dry run localonly" '^would delete +local +localonly +\(merged: ancestor'
has "dry run remoteonly" '^would delete +remote +remoteonly +\(merged: ancestor'
has "dry run main kept" '^keep +local +main +\(protected'
has "dry run benchmarks kept" '^keep +local +benchmarks +\(protected'
has "dry run benchmarks remote kept" '^keep +remote +benchmarks +\(protected'
has "dry run wt kept" '^keep +local +wt +\(protected: checked out in worktree'
has "dry run wt remote kept" '^keep +remote +wt +\(protected: checked out in worktree'
has "dry run unmerged section" '^kept \(unmerged work\)'
has "dry run unmerged count" '^ +local +unmerged +2 commits'
hasnt "dry run deleted" '^deleted '

# --- --delete ---
run --delete
[[ $status -eq 0 ]] || fail "--delete exit $status: $out"
has "delete ff local" '^deleted +local +ff '
has "delete rebased remote" '^deleted +remote +rebased '
has "delete remoteonly" '^deleted +remote +remoteonly '
want="benchmarks main unmerged wt"
[[ $(local_heads) == "$want" ]] || fail "local after --delete: '$(local_heads)' want '$want'"
[[ $(remote_heads) == "$want" ]] || fail "remote after --delete: '$(remote_heads)' want '$want'"
has "delete unmerged listed" '^ +remote +unmerged +2 commits'

# --- a refused remote deletion is reported and does not stop the run ---
cd "$tmp/work"
git branch stuck main
git push -q origin stuck
git branch zzz-after main
git push -q origin zzz-after
cat >"$tmp/origin.git/hooks/pre-receive" <<'HOOK'
#!/bin/sh
while read -r old new ref; do
    if [ "$ref" = refs/heads/stuck ] && [ "$new" = 0000000000000000000000000000000000000000 ]; then
        echo "deletion refused" >&2
        exit 1
    fi
done
HOOK
chmod +x "$tmp/origin.git/hooks/pre-receive"
run --delete
[[ $status -eq 1 ]] || fail "refused deletion: exit $status (want 1)"
has "refused reported" '^FAILED to delete +remote +stuck'
has "after refusal continues" '^deleted +remote +zzz-after '
want="benchmarks main stuck unmerged wt"
[[ $(remote_heads) == "$want" ]] || fail "remote after refusal: '$(remote_heads)' want '$want'"
[[ $(local_heads) == "benchmarks main unmerged wt" ]] || fail "local after refusal: '$(local_heads)'"

# --- more cases, on fresh repository pairs ---
# mkpair <name> — origin.git + work clone with one commit on main, pushed;
# leaves the shell in the work clone.
mkpair() {
    git init -q --bare -b main "$tmp/$1-origin.git"
    git clone -q "$tmp/$1-origin.git" "$tmp/$1" 2>/dev/null
    cd "$tmp/$1"
    git checkout -q -b main 2>/dev/null || true
    commit base.txt 0 base
    git push -q origin main
    wd=$tmp/$1
}
rheads() { git -C "$tmp/$1-origin.git" for-each-ref --format='%(refname:short)' refs/heads | sort | paste -sd' '; }

# behind (strictly behind the base), a slash name, a non-main checked-out
# branch, --keep, detached HEAD.
mkpair x
git branch behind main
git checkout -q -b feat/slash main
commit s.txt 1 slash
git checkout -q -b cur main
git checkout -q -b keepme main
git checkout -q main
git merge -q --ff-only feat/slash
commit m.txt 1 "main moves on"
git push -q origin main behind feat/slash cur keepme
git checkout -q cur
run --keep keepme
[[ $status -eq 0 ]] || fail "x dry run exit $status: $out"
has "behind" '^would delete +local +behind +\(merged: ancestor'
has "slash" '^would delete +remote +feat/slash +\(merged: ancestor'
has "current branch kept" '^keep +local +cur +\(protected: checked out'
has "--keep" '^keep +local +keepme +\(protected: kept by name'
has "--keep remote" '^keep +remote +keepme +\(protected: kept by name'
git checkout -q --detach
run
[[ $status -eq 0 ]] || fail "detached exit $status: $out"
has "detached: cur no longer protected" '^would delete +local +cur +'
run --delete --keep keepme
[[ $status -eq 0 ]] || fail "x --delete exit $status: $out"
[[ $(git for-each-ref --format='%(refname:short)' refs/heads | sort | paste -sd' ') == "keepme main" ]] ||
    fail "x local after delete"
[[ $(rheads x) == "keepme main" ]] || fail "x remote after delete: $(rheads x)"

# --base: judged against another branch.
mkpair y
git checkout -q -b develop main
commit d.txt 1 develop
git checkout -q -b ondev develop
git checkout -q -b sidemain main
commit sm.txt 1 sidemain
git checkout -q main
git push -q origin develop ondev sidemain
run --base develop
[[ $status -eq 0 ]] || fail "--base exit $status: $out"
has "--base" '^base: origin/develop'
has "--base ondev" '^would delete +remote +ondev +\(merged: ancestor of origin/develop'
has "--base develop kept" '^keep +remote +develop +\(protected: base branch'
has "--base unmerged" '^ +remote +sidemain +1 commit$'

# A remote branch whose objects are not local (--single-branch clone):
# unknown, kept, exit 1, even with --delete.
mkpair z
git checkout -q -b other main
commit o.txt 1 other
git checkout -q -b mergedb main
git push -q origin other mergedb
git clone -q --single-branch --branch main "$tmp/z-origin.git" "$tmp/z2" 2>/dev/null
wd=$tmp/z2
run --delete
[[ $status -eq 1 ]] || fail "single-branch exit $status (want 1): $out"
has "single-branch other" '^unknown +remote +other +\(unknown: kept'
has "single-branch merged one" '^unknown +remote +mergedb +\(unknown: kept'
hasnt "single-branch deleted" '^deleted '
[[ $(rheads z) == "main mergedb other" ]] || fail "single-branch deleted something: $(rheads z)"

# A failing fetch stops the run before anything is evaluated.
wd=$tmp/work
run --remote nonexist --delete
[[ $status -eq 2 ]] || fail "bad remote exit $status (want 2): $out"
hasnt "bad remote" '^(would delete|deleted|keep) '

if [[ $failures -ne 0 ]]; then
    echo "$failures failure(s)" >&2
    exit 1
fi
echo "ok: cleanup-merged-branches"
