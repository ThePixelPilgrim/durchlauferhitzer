#!/usr/bin/env bash
# Delete branches that are already merged into main, locally and on the
# remote. Dry run unless --delete is given.
#
# usage: scripts/cleanup-merged-branches.sh [--delete] [--remote <name>]
#                                       [--base <branch>] [--keep <name>]...
#
# It fetches (with --prune) first and compares against <remote>/<base>,
# never against a local branch. A branch is merged when all its commits
# are on the base (ancestor) or patch-equivalent to commits on it
# (`git cherry` shows no `+`; covers branches rebased into main).
# Never deleted: the base, the --keep names (default: main benchmarks;
# --keep adds to them), the checked-out branch, branches checked out in
# any worktree. Branches with other commits are listed as unmerged work.
# `git cherry` ignores merge commits, so a branch whose only unique
# content is a merge commit counts as merged.
# Fails closed: a branch that cannot be checked (commit missing locally,
# e.g. a --single-branch clone, or a git error) is reported "unknown:
# kept", never deleted, and makes the exit status 1. Remote branches are
# judged by their fetched remote-tracking refs, and deleted with a lease
# on the checked sha, so a push in between is not lost.
# A refused deletion is reported and the run goes on; the exit status is
# 1 if any deletion failed or any branch was unknown, 2 on a usage error
# (including a failed fetch).
set -euo pipefail

usage() {
    sed -n '2,/^set -/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
}

remote=origin base=main delete=0
keep=(main benchmarks)
while [[ $# -gt 0 ]]; do
    case $1 in
        --delete) delete=1 ;;
        --remote | --base | --keep)
            [[ $# -ge 2 ]] || { echo "$1 needs a value" >&2; exit 2; }
            case $1 in
                --remote) remote=$2 ;;
                --base) base=$2 ;;
                --keep) keep+=("$2") ;;
            esac
            shift ;;
        -h | --help) usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
    esac
    shift
done
keep+=("$base")

echo "fetching $remote ..."
git fetch --prune --quiet "$remote" || { echo "fetch of $remote failed" >&2; exit 2; }
basetip=$(git rev-parse --verify --quiet "refs/remotes/$remote/$base^{commit}") ||
    { echo "no $remote/$base after fetch" >&2; exit 2; }
echo "base: $remote/$base ($(git rev-parse --short "$basetip"))"

# Branches checked out in any worktree (the main one included).
declare -A wt=()
name= path=
while IFS= read -r line; do
    case $line in
        "worktree "*) path=${line#worktree } ;;
        "branch refs/heads/"*) wt[${line#branch refs/heads/}]=$path ;;
    esac
done < <(git worktree list --porcelain)
current=$(git symbolic-ref --quiet --short HEAD || true)

failed=0 unmerged_lines=() reason=

# protected <name> — sets $why and returns 0 when the branch must stay.
protected() {
    local k
    for k in "${keep[@]}"; do
        if [[ $1 == "$k" ]]; then why="protected: $([[ $1 == "$base" ]] && echo base branch || echo kept by name)"; return 0; fi
    done
    if [[ -n $current && $1 == "$current" ]]; then why="protected: checked out"; return 0; fi
    if [[ -v wt[$1] ]]; then why="protected: checked out in worktree ${wt[$1]}"; return 0; fi
    return 1
}

# merged <rev> — 0: merged (sets $rule), 1: unmerged (sets $count),
# 2: could not be checked (sets $reason). Never reports an error as merged.
merged() {
    local rc=0 cherry
    git cat-file -e "$1^{commit}" 2>/dev/null ||
        { reason="unknown: kept, commit not available locally"; return 2; }
    git merge-base --is-ancestor "$1" "$basetip" || rc=$?
    case $rc in
        0) rule="merged: ancestor of $remote/$base"; return 0 ;;
        1) ;;
        *) reason="unknown: kept, merge-base failed ($rc)"; return 2 ;;
    esac
    cherry=$(git cherry "$basetip" "$1") ||
        { reason="unknown: kept, git cherry failed"; return 2; }
    count=$(grep -c '^+' <<<"$cherry" || true)
    if [[ $count -eq 0 ]]; then
        rule="merged: patch-equivalent on $remote/$base"; return 0
    fi
    return 1
}

# handle <local|remote> <name> <rev> [lease-sha]
handle() {
    local scope=$1 name=$2 rev=$3 lease=${4:-} verb=deleted out rc=0
    if protected "$name"; then
        printf '%-16s %-6s %s  (%s)\n' keep "$scope" "$name" "$why"
        return
    fi
    merged "$rev" || rc=$?
    if [[ $rc -eq 2 ]]; then
        printf '%-16s %-6s %s  (%s)\n' unknown "$scope" "$name" "$reason"
        failed=1
        return
    elif [[ $rc -eq 1 ]]; then
        unmerged_lines+=("$(printf '  %-6s %s  %s commit%s' "$scope" "$name" "$count" "$([[ $count -eq 1 ]] || echo s)")")
        return
    fi
    if [[ $delete -eq 0 ]]; then
        printf '%-16s %-6s %s  (%s)\n' "would delete" "$scope" "$name" "$rule"
        return
    fi
    if [[ $scope == local ]]; then
        out=$(git branch -D -- "$name" 2>&1) || verb=failed
    else
        out=$(git push --quiet --force-with-lease="refs/heads/$name:$lease" "$remote" ":refs/heads/$name" 2>&1) || verb=failed
    fi
    if [[ $verb == failed ]]; then
        printf '%-16s %-6s %s  (%s)\n' "FAILED to delete" "$scope" "$name" "$(tr '\n' ' ' <<<"$out")"
        failed=1
    else
        printf '%-16s %-6s %s  (%s)\n' deleted "$scope" "$name" "$rule"
    fi
}

# Capture the producers first so a failing git stops the run (set -e).
refs=$(git for-each-ref --format='%(refname)' refs/heads)
while read -r ref; do
    [[ -n $ref ]] || continue
    handle local "${ref#refs/heads/}" "$ref"
done <<<"$refs"

heads=$(git ls-remote --heads "$remote")
while IFS=$'\t' read -r sha ref; do
    [[ -n $ref ]] || continue
    name=${ref#refs/heads/}
    # Judge the fetched remote-tracking ref, and only if it is the sha the
    # remote reports now; otherwise the branch is unknown.
    if tracked=$(git rev-parse --verify --quiet "refs/remotes/$remote/$name^{commit}") && [[ $tracked == "$sha" ]]; then
        handle remote "$name" "refs/remotes/$remote/$name" "$sha"
    elif protected "$name"; then
        printf '%-16s %-6s %s  (%s)\n' keep remote "$name" "$why"
    else
        printf '%-16s %-6s %s  (%s)\n' unknown remote "$name" "unknown: kept, no up-to-date remote-tracking ref"
        failed=1
    fi
done <<<"$heads"

if [[ ${#unmerged_lines[@]} -gt 0 ]]; then
    echo
    echo "kept (unmerged work):"
    printf '%s\n' "${unmerged_lines[@]}"
fi
[[ $delete -eq 1 ]] || echo "dry run: nothing deleted (use --delete)"
exit "$failed"
