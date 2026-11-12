#!/bin/sh
# Temporary (stage 10 Task 2): which shell prints a job notice for a
# killed background job, and which variant silences it. Each argument
# is a shell command line (e.g. "bash --posix").
kid='$(cat /proc/$p/task/$p/children)'
for shell in "$@"; do
	echo "=== shell: $shell"
	$shell -c 'case :${SHELLOPTS:-}: in *:posix:*) m=on ;; *) m=off ;; esac; echo "BASH_VERSION=${BASH_VERSION:-none} posix=$m"; ls -l /bin/sh'
	run() {
		echo "--- $1"
		out=$($shell -c "$2" 2>&1)
		printf '%s\n' "$out" | sed 's/^/    | /'
	}
	run "a as is" '( trap - EXIT; exec sleep 100 ) & p=$!; kill -9 $p; sleep 1; wait $p 2>/dev/null; echo "wait=$?"'
	run "a2 as is, SIGTERM" '( trap - EXIT; exec sleep 100 ) & p=$!; kill $p; sleep 1; wait $p 2>/dev/null; echo "wait=$?"'
	run "b disown" '( trap - EXIT; exec sleep 100 ) & p=$!; disown $p; echo "disown=$?"; kill -9 $p; sleep 1; wait $p 2>/dev/null; echo "wait=$?"'
	run "c grace sleep 2>/dev/null" '( trap - EXIT; exec sleep 100 ) & p=$!; kill -9 $p; { sleep 1; } 2>/dev/null; wait $p 2>/dev/null; echo "wait=$?"'
	run "d set +m" 'set +m; ( trap - EXIT; exec sleep 100 ) & p=$!; kill -9 $p; sleep 1; wait $p 2>/dev/null; echo "wait=$?"'
	run "e set +b" 'set +b; ( trap - EXIT; exec sleep 100 ) & p=$!; kill -9 $p; sleep 1; wait $p 2>/dev/null; echo "wait=$?"'
	run "f non-exec subshell, grandchild killed" '( trap - EXIT; sleep 100; exit $? ) & p=$!; sleep 0.3; k='"$kid"'; echo "child=$k"; kill -9 $k; sleep 1; wait $p 2>/dev/null; echo "wait=$?"'
	run "g kill, then wait at once (no sleep between)" '( trap - EXIT; exec sleep 100 ) & p=$!; kill -9 $p; wait $p 2>/dev/null; echo "wait=$?"; sleep 1'
	run "h whole region in { } 2>/dev/null" '( trap - EXIT; exec sleep 100 ) & p=$!; { kill -9 $p; sleep 1; wait $p; } 2>/dev/null; echo "wait=$?"'
done
