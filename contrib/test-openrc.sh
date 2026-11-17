#!/bin/sh
# test-openrc.sh: the OpenRC script end to end (FR-PROC-07, FR-PROC-07a).
# POSIX sh.
#
# usage: test-openrc.sh <binary>
#
# As root, on a throwaway system with OpenRC (CI: the Alpine container);
# it installs into /etc/init.d, /etc/conf.d, /usr/sbin, /usr/libexec and
# /srv/durchlauferhitzer, and serves on 127.0.0.1:$PORT (default 18080).
#
#  1. Installs the script, its conf.d, the reload helper and <binary>;
#     a config with one static root, `user nobody`.
#  2. Marks OpenRC as booted (/run/openrc/softlevel; `rc_provide` gives
#     `net`, which a container has no service for).
#  3. `rc-service durchlauferhitzer start`, a request.
#  4. A slow client holds a connection with half a request head.
#  5. Replaces the binary in place by a copy of <binary> (a new inode:
#     the old process's exe then reads " (deleted)"), points the config at
#     a second root, `rc-service durchlauferhitzer reload`; the request
#     gets the second root's answer from the new process. The old process
#     still drains (it holds the slow client).
#  6. `rc-service durchlauferhitzer status` reports started; the new
#     process's descriptors are listed and must be stdin (/dev/null), the
#     log, the root, its one listener and its anonymous inodes (epoll,
#     signalfd, eventfds): nothing inherited from the helper or OpenRC.
#  7. A crash after the reload: `kill -9` of the new process while the
#     old one still drains; `status` reports crashed with exit status 32
#     (FR-PROC-07a: the script does not restart the server); `restart`
#     ends the draining old process (stop_post) and serves again; no
#     other server process and no draining list are left.
#  8. A crash without a reload: the same on the restarted process.
#  9. `rc-service durchlauferhitzer stop` ends the server; no server
#     process and no draining list are left.
#
# Exit 0 when every step passed; otherwise 1, with the step and the log.

set -eu

me=test-openrc
[ $# -eq 1 ] || {
	echo "usage: $me <binary>" >&2
	exit 2
}
binary=$1
here=$(cd "$(dirname "$0")" && pwd)
port=${PORT:-18080}
svc=durchlauferhitzer
bin=/usr/sbin/$svc
conf=/etc/$svc/$svc.conf
log=/var/log/$svc.log
pidfile=/run/$svc.pid
srv=/srv/$svc
slow=

fail() {
	echo "$me: FAILED: $*" >&2
	echo "--- $log" >&2
	cat "$log" >&2 2>/dev/null || true
	exit 1
}

step() {
	echo "== $*"
}

# The state of a pid ("R", "S", ...), empty if there is none; a zombie
# counts as gone.
running() {
	s=$(sed -n 's/^.*) \(.\).*$/\1/p' "/proc/$1/stat" 2>/dev/null)
	[ -n "$s" ] && [ "$s" != Z ]
}

get() {
	curl -sS --max-time 5 -H 'Host: localhost' "http://127.0.0.1:$port/a.txt"
}

# Waits (at most 5 s) until the server answers $1; $2 names the step.
answers() {
	n=0
	until got=$(get 2>/dev/null); do
		n=$((n + 1))
		[ "$n" -lt 50 ] || fail "$2: no answer"
		sleep 0.1
	done
	[ "$got" = "$1" ] || fail "$2: got '$got', not '$1'"
}

# The pids of the running server processes: those whose executable is
# $bin (" (deleted)" allowed: the binary is replaced in place).
servers() {
	for p in /proc/[0-9]*; do
		e=$(readlink "$p/exe" 2>/dev/null) || continue
		[ "${e% (deleted)}" = "$bin" ] && running "${p#/proc/}" &&
			printf '%s\n' "${p#/proc/}"
	done
	return 0
}

# FR-PROC-07a: kills the serving process (the pid file's) with SIGKILL;
# `status` must report crashed with exit status 32 (polled for at most
# 5 s, until pid 1 has reaped it), then `restart` must serve $1 from a
# new process, the only server process left, with no draining list.
# $2 names the step.
crash_and_restart() {
	victim=$(cat "$pidfile")
	kill -KILL "$victim" || fail "$2: cannot kill -9 pid $victim"
	n=0
	while :; do
		st=0
		out=$(rc-service "$svc" status 2>&1) || st=$?
		[ "$st" -eq 0 ] || break
		n=$((n + 1))
		[ "$n" -lt 50 ] || fail "$2: status still reports success 5 s after kill -9 of $victim: $out"
		sleep 0.1
	done
	echo "$out (exit status $st)"
	case $out in
	*crashed*) ;;
	*) fail "$2: status does not report crashed: $out" ;;
	esac
	[ "$st" -eq 32 ] || fail "$2: status exit status $st, not 32"
	rc-service "$svc" restart || fail "$2: restart"
	cur=$(cat "$pidfile") || fail "$2: no pid file after restart"
	[ "$cur" != "$victim" ] || fail "$2: the pid file still names $victim"
	answers "$1" "$2"
	echo "pid $cur answers '$got' after restart"
	left=$(servers)
	[ "$left" = "$cur" ] || fail "$2: server processes after restart: $(echo $left), not $cur alone"
	[ ! -e "$pidfile.draining" ] || fail "$2: $pidfile.draining is left: $(cat "$pidfile.draining")"
}

config() {
	cat <<EOF
listen 127.0.0.1:$port;
user nobody;
limits { header_timeout 60s; }
containment { landlock required; seccomp required; seccomp_action kill; }
host localhost { path / { static $srv/$1; } }
EOF
}

cleanup() {
	[ -n "$slow" ] && kill "$slow" 2>/dev/null
	return 0
}
trap cleanup EXIT

step "1. install"
install -D -m 755 "$binary" "$bin"
install -D -m 755 "$here/openrc/$svc" "/etc/init.d/$svc"
install -D -m 644 "$here/openrc/$svc.confd" "/etc/conf.d/$svc"
install -D -m 755 "$here/$svc-reload" "/usr/libexec/$svc/$svc-reload"
for answer in one two; do
	mkdir -p "$srv/$answer"
	echo "$answer" >"$srv/$answer/a.txt"
done
chmod -R a+rX "$srv"
mkdir -p "/etc/$svc"
config one >"$conf"

step "2. OpenRC"
mkdir -p /run/openrc
touch /run/openrc/softlevel
grep -q '^rc_provide=' /etc/rc.conf 2>/dev/null ||
	echo 'rc_provide="loopback net"' >>/etc/rc.conf
openrc default

step "3. start"
rc-service "$svc" start || fail "start"
old=$(cat "$pidfile")
answers one start
echo "pid $old answers '$got'"

step "4. a slow client"
{
	printf 'GET /a.txt HTTP/1.1\r\nHost: localhost\r\n'
	sleep 60
} | nc 127.0.0.1 "$port" >/dev/null &
slow=$!
sleep 0.5

step "5. replace the binary, reload"
cp "$binary" "$bin.new"
chmod 755 "$bin.new"
mv -f "$bin.new" "$bin"
config two >"$conf"
rc-service "$svc" reload || fail "reload"
new=$(cat "$pidfile")
[ "$new" != "$old" ] || fail "the pid file still names $old"
got=$(get) || fail "no answer after reload"
[ "$got" = two ] || fail "reload: got '$got', not 'two'"
echo "pid $new answers '$got'"
running "$old" || fail "the old process $old no longer drains the slow client"
echo "pid $old drains, exe $(readlink "/proc/$old/exe")"

step "6. status, descriptors"
status=$(rc-service "$svc" status) || fail "status: $status"
echo "$status"
case $status in
*started*) ;;
*) fail "status does not report started" ;;
esac
ls -l "/proc/$new/fd"
sockets=0
for fd in /proc/"$new"/fd/*; do
	n=${fd##*/}
	target=$(readlink "$fd")
	case $n:$target in
	0:/dev/null | 1:"$log" | 2:"$log") ;;
	[0-2]:*) fail "fd $n is $target" ;;
	*:socket:*) sockets=$((sockets + 1)) ;;
	*:anon_inode:*) ;;
	*:"$srv/two") ;;
	*) fail "unexpected descriptor $n: $target" ;;
	esac
done
[ "$sockets" -eq 1 ] || fail "$sockets sockets, not the one listener"

step "7. a crash after the reload"
running "$old" || fail "the old process $old no longer drains before the crash"
crash_and_restart two "crash after reload"
if running "$old"; then fail "the draining old process $old still runs after restart"; fi
if running "$new"; then fail "the killed process $new still runs"; fi
echo "pids $new (killed) and $old (draining) are gone"

step "8. a crash without a reload"
crash_and_restart two "crash without reload"

step "9. stop"
cur=$(cat "$pidfile")
rc-service "$svc" stop || fail "stop"
n=0
while [ -n "$(servers)" ]; do
	n=$((n + 1))
	[ "$n" -lt 50 ] || fail "still running after stop: $(echo $(servers))"
	sleep 0.1
done
[ ! -e "$pidfile.draining" ] || fail "$pidfile.draining is left"
echo "pid $cur is gone, no server process left"
echo "$me: ok"
