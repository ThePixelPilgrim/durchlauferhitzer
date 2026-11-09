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
# A refused deletion is reported and the run goes on; the exit status is
# 1 if any deletion failed, 2 on a usage error.
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
git fetch --prune --quiet "$remote"
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

failed=0 unmerged_lines=()

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

# merged <rev> — sets $rule (merged) or $count (commits not on the base).
merged() {
    local cherry
    if git merge-base --is-ancestor "$1" "$basetip"; then
        rule="merged: ancestor of $remote/$base"; return 0
    fi
    cherry=$(git cherry "$basetip" "$1")
    count=$(grep -c '^+' <<<"$cherry" || true)
    if [[ $count -eq 0 ]]; then
        rule="merged: patch-equivalent on $remote/$base"; return 0
    fi
    return 1
}

# handle <local|remote> <name> <rev>
handle() {
    local scope=$1 name=$2 rev=$3 verb=deleted out
    if protected "$name"; then
        printf '%-16s %-6s %s  (%s)\n' keep "$scope" "$name" "$why"
        return
    fi
    if ! merged "$rev"; then
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
        out=$(git push --quiet "$remote" --delete "$name" 2>&1) || verb=failed
    fi
    if [[ $verb == failed ]]; then
        printf '%-16s %-6s %s  (%s)\n' "FAILED to delete" "$scope" "$name" "$(tr '\n' ' ' <<<"$out")"
        failed=1
    else
        printf '%-16s %-6s %s  (%s)\n' deleted "$scope" "$name" "$rule"
    fi
}

while read -r ref; do
    handle local "${ref#refs/heads/}" "$ref"
done < <(git for-each-ref --format='%(refname)' refs/heads)

while IFS=$'\t' read -r sha ref; do
    handle remote "${ref#refs/heads/}" "$sha"
done < <(git ls-remote --heads "$remote")

if [[ ${#unmerged_lines[@]} -gt 0 ]]; then
    echo
    echo "kept (unmerged work):"
    printf '%s\n' "${unmerged_lines[@]}"
fi
[[ $delete -eq 1 ]] || echo "dry run: nothing deleted (use --delete)"
exit "$failed"
