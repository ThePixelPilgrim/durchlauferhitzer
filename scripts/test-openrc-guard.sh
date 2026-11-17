#!/bin/sh
# Self-test of the OpenRC script's pid-file guard (FR-PROC-07a): its
# `stop` and `status` against a pid file naming the server, a positively
# foreign process, or a process whose executable cannot be read. It
# sources contrib/openrc/durchlauferhitzer with OpenRC's functions stubbed
# (no OpenRC needed), under every shell present of sh, dash, bash and
# busybox ash. POSIX sh.
#
# The unknown case needs a process whose /proc/<pid>/exe the check cannot
# read:
#  - as root: a `sleep` of uid 65534, checked under
#    `setpriv --bounding-set=-sys_ptrace` (root without CAP_SYS_PTRACE);
#  - as another user: pid 1, a process of another uid.
# If neither works it is skipped with a message, unless
# TEST_OPENRC_GUARD_REQUIRE=1 (CI), which makes the skip a failure.

set -u

here=$(cd "$(dirname "$0")" && pwd)
script=$here/../contrib/openrc/durchlauferhitzer
failures=0
pids=
tmp=$(mktemp -d) || exit 1

cleanup() {
	for p in $pids; do
		kill "$p" 2>/dev/null
	done
	rm -rf "$tmp"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

fail() {
	echo "FAIL: $*" >&2
	failures=$((failures + 1))
}

alive() {
	s=$(sed -n 's/^.*) \(.\).*$/\1/p' "/proc/$1/stat" 2>/dev/null)
	[ -n "$s" ] && [ "$s" != Z ]
}

# The harness: sources the script with OpenRC's functions stubbed, sets
# the pid file and the current `command`, runs one action and prints its
# exit status. OpenRC's stored values (service_get_value/set_value) live
# in files under $values; the default_* calls are logged to $calls.
cat >"$tmp/harness.sh" <<'EOF'
# usage: <shell> harness.sh <script> <pidfile> <command> <values> <calls> <action>
values=$4 calls=$5
ewarn() { echo "ewarn: $*"; }
eerror() { echo "eerror: $*"; }
einfo() { :; }
ebegin() { :; }
eend() { return "${1:-0}"; }
service_started() { return 0; }
service_get_value() { cat "$values/$1" 2>/dev/null; }
service_set_value() { printf '%s\n' "$2" >"$values/$1"; }
default_stop() { echo default_stop >>"$calls"; }
default_status() { echo default_status >>"$calls"; return 0; }
RC_SVCNAME=durchlauferhitzer
. "$1"
pidfile=$2 command=$3
"$6"
echo "rc=$?"
EOF

# Real executables at distinct paths: the "server" is a copy of sleep
# named sleep (busybox picks its applet by that name).
sys_sleep=$(command -v sleep)
mkdir -p "$tmp/a" "$tmp/b" "$tmp/link"
cp "$(readlink -f "$sys_sleep")" "$tmp/a/sleep"
cp "$(readlink -f "$sys_sleep")" "$tmp/b/sleep"
chmod 755 "$tmp/a/sleep" "$tmp/b/sleep"
[ "$(readlink -f "$sys_sleep")" != "$tmp/a/sleep" ] || {
	echo "FAIL: setup: the system sleep is the copy" >&2
	exit 1
}

# start <command...>: starts it in the background; its pid in $started.
start() {
	"$@" 300 &
	started=$!
	pids="$pids $started"
	sleep 0.2 2>/dev/null || sleep 1
	alive "$started" || {
		echo "FAIL: setup: $* did not start" >&2
		exit 1
	}
}

# run <shell> <wrapper> <pid> <command> <action> <recorded command>: one
# action on a fresh pid file and value store; sets $out, $called and
# $kept (yes/no).
run() {
	rm -rf "$tmp/values" "$tmp/calls"
	mkdir "$tmp/values"
	: >"$tmp/calls"
	[ -n "$6" ] && printf '%s\n' "$6" >"$tmp/values/command"
	[ -n "${exe_record-}" ] && printf '%s\n' "$exe_record" >"$tmp/values/server_exe"
	printf '%s\n' "$3" >"$tmp/pid"
	# shellcheck disable=SC2086 # the wrapper is zero or more words
	out=$($2 $1 "$tmp/harness.sh" "$script" "$tmp/pid" "$4" "$tmp/values" "$tmp/calls" "$5" 2>&1)
	called=$(cat "$tmp/calls")
	kept=no
	[ -f "$tmp/pid" ] && kept=yes
}

# expect_default <what> ...: run's arguments; OpenRC's own function ran,
# the pid file is kept and the process still runs.
expect_default() {
	what=$1
	shift
	for action in status stop; do
		run "$@" "$action" "$recorded"
		case $out in
		*"rc=0") ;;
		*) fail "$sh: $what: $action: not rc=0: $out" ;;
		esac
		[ "$called" = "default_$action" ] || fail "$sh: $what: $action: default_$action not called ($called): $out"
		[ "$kept" = yes ] || fail "$sh: $what: $action: the pid file was removed"
		alive "$3" || fail "$sh: $what: $action: pid $3 no longer runs"
	done
}

# expect_stranger <what> ...: status crashed (32); stop warns, removes the
# pid file, calls no default and signals nothing.
expect_stranger() {
	what=$1
	shift
	run "$@" status "$recorded"
	case $out in
	*crashed*"rc=32") ;;
	*) fail "$sh: $what: status: not crashed with rc=32: $out" ;;
	esac
	[ -z "$called" ] || fail "$sh: $what: status: $called called"
	run "$@" stop "$recorded"
	case $out in
	*ewarn:*"rc=0") ;;
	*) fail "$sh: $what: stop: no warning and rc=0: $out" ;;
	esac
	[ -z "$called" ] || fail "$sh: $what: stop: $called called"
	[ "$kept" = no ] || fail "$sh: $what: stop: the pid file was kept"
	alive "$3" || fail "$sh: $what: stop: pid $3 no longer runs"
}

# The unknown case's target and the wrapper that cannot read its exe.
unknown_pid= unknown_wrap= skip=
if [ "$(id -u)" -eq 0 ]; then
	if command -v setpriv >/dev/null 2>&1; then
		start setpriv --reuid=65534 --regid=65534 --clear-groups "$sys_sleep"
		unknown_pid=$started
		unknown_wrap="setpriv --bounding-set=-sys_ptrace"
	else
		skip="root without setpriv"
	fi
else
	unknown_pid=1
	unknown_wrap=
fi
if [ -n "$unknown_pid" ] &&
	$unknown_wrap readlink "/proc/$unknown_pid/exe" >/dev/null 2>&1; then
	skip="pid $unknown_pid's executable is readable here"
	unknown_pid=
fi

start "$tmp/a/sleep"
server=$started
start "$sys_sleep"
stranger=$started
ln -s "$tmp/a/sleep" "$tmp/link/sleep"
start "$tmp/link/sleep"
linked=$started

shells=
for s in sh dash bash; do
	command -v "$s" >/dev/null 2>&1 && shells="$shells $s"
done
command -v busybox >/dev/null 2>&1 && busybox ash -c : 2>/dev/null &&
	shells="$shells busybox_ash"
[ -n "$shells" ] || {
	echo "FAIL: no shell found" >&2
	exit 1
}

for sh in $shells; do
	shell=$sh
	[ "$sh" = busybox_ash ] && shell="busybox ash"
	exe_record=

	recorded=$tmp/a/sleep
	expect_default "own server" "$shell" "" "$server" "$tmp/a/sleep"
	expect_stranger "a foreign process" "$shell" "" "$stranger" "$tmp/a/sleep"

	# `command` edited in conf.d after the start: the recorded one counts.
	recorded=$tmp/a/sleep
	expect_default "recorded command differs" "$shell" "" "$server" "$tmp/b/sleep"

	# A symlinked command switched to another binary after the start:
	# the executable recorded at start (start_post) counts.
	rm -f "$tmp/link/sleep"
	ln -s "$tmp/a/sleep" "$tmp/link/sleep"
	recorded=$tmp/link/sleep
	run "$shell" "" "$linked" "$tmp/link/sleep" start_post "$recorded"
	exe_record=$(cat "$tmp/values/server_exe" 2>/dev/null)
	[ "$exe_record" = "$tmp/a/sleep" ] || fail "$sh: start_post recorded '$exe_record', not $tmp/a/sleep: $out"
	rm -f "$tmp/link/sleep"
	ln -s "$tmp/b/sleep" "$tmp/link/sleep"
	expect_default "symlinked command switched" "$shell" "" "$linked" "$tmp/link/sleep"
	exe_record=

	if [ -n "$unknown_pid" ]; then
		recorded=$tmp/a/sleep
		expect_default "unreadable executable" "$shell" "$unknown_wrap" "$unknown_pid" "$tmp/a/sleep"
	fi
done

if [ -n "$skip" ]; then
	if [ "${TEST_OPENRC_GUARD_REQUIRE-}" = 1 ]; then
		fail "the unknown case cannot run: $skip"
	else
		echo "test-openrc-guard.sh: skipped: the unknown case: $skip" >&2
	fi
fi
if [ "$failures" -gt 0 ]; then
	echo "test-openrc-guard.sh self-test: $failures case(s) failed" >&2
	exit 1
fi
echo "test-openrc-guard.sh self-test: all cases passed ($(echo $shells))"
