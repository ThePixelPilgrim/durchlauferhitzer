#!/bin/sh
# Temporary (stage 10 Task 2): which shell prints a job notice for a
# killed background job, and which variant silences it. Each argument
# is a shell command line (e.g. "bash --posix"). Each variant runs as a
# script file, as the reload helper does, with the helper's traps, and
# the job is killed by another process, as the test does.
tmp=$(mktemp -d)
pre='trap "echo cleanup >/dev/null" EXIT
trap "" HUP INT TERM
xkill() { sh -c "kill -$1 $2"; }
'
for shell in "$@"; do
	echo "=== shell: $shell"
	$shell -c 'case :${SHELLOPTS:-}: in *:posix:*) m=on ;; *) m=off ;; esac; echo "BASH_VERSION=${BASH_VERSION:-none} posix=$m"; ls -l /bin/sh'
	run() {
		echo "--- $1"
		printf '%s%s\n' "$pre" "$2" >"$tmp/v.sh"
		out=$($shell "$tmp/v.sh" 2>&1)
		printf '%s\n' "$out" | sed 's/^/    | /'
	}
	run "a as is" '( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
xkill 9 $p
sleep 1
wait $p 2>/dev/null; echo "wait=$?"'
	run "a2 as is, SIGTERM" '( trap - EXIT TERM; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
xkill TERM $p
sleep 1
wait $p 2>/dev/null; echo "wait=$?"'
	run "a3 as is, no traps" 'trap - EXIT; ( exec sleep 100 >/dev/null 2>&1 ) &
p=$!
xkill 9 $p
sleep 1
wait $p 2>/dev/null; echo "wait=$?"'
	run "b disown" '( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
disown $p; echo "disown=$?"
xkill 9 $p
sleep 1
wait $p 2>/dev/null; echo "wait=$?"'
	run "c grace sleep 2>/dev/null" '( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
xkill 9 $p
{ sleep 1; } 2>/dev/null
wait $p 2>/dev/null; echo "wait=$?"'
	run "c2 grace sleep 2>/dev/null, then a command" '( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
xkill 9 $p
{ sleep 1; } 2>/dev/null
kill -0 $p 2>/dev/null; echo "alive=$?"
wait $p 2>/dev/null; echo "wait=$?"'
	run "d set +m" 'set +m
( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
xkill 9 $p
sleep 1
wait $p 2>/dev/null; echo "wait=$?"'
	run "e set +b" 'set +b
( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
xkill 9 $p
sleep 1
wait $p 2>/dev/null; echo "wait=$?"'
	run "f non-exec subshell, grandchild killed" '( trap - EXIT; sleep 100 >/dev/null 2>&1; exit $? ) &
p=$!
sleep 0.3
k=$(cat /proc/$p/task/$p/children); echo "child=$k"
xkill 9 $k
sleep 1
wait $p 2>/dev/null; echo "wait=$?"'
	run "g killed while in wait" '( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
(sleep 0.5; kill -9 $p) &
wait $p 2>/dev/null; echo "wait=$?"
sleep 1'
	run "h whole region in { } 2>/dev/null" '( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
{ xkill 9 $p; sleep 1; wait $p; } 2>/dev/null; echo "wait=$?"'
	run "i stderr of the script saved and restored around the region" '( trap - EXIT; exec sleep 100 >/dev/null 2>&1 ) &
p=$!
exec 9>&2 2>/dev/null
xkill 9 $p
sleep 1
wait $p; w=$?
exec 2>&9 9>&-
echo "wait=$w"'
done
rm -rf "$tmp"
