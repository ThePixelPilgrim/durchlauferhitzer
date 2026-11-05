#!/bin/sh
# test-openrc.sh: the OpenRC script end to end (FR-PROC-07). POSIX sh.
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
#  7. `rc-service durchlauferhitzer stop` ends the new process and the
#     draining old one.
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
n=0
until got=$(get 2>/dev/null); do
	n=$((n + 1))
	[ "$n" -lt 50 ] || fail "no answer after start"
	sleep 0.1
done
[ "$got" = one ] || fail "start: got '$got', not 'one'"
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

step "7. stop"
rc-service "$svc" stop || fail "stop"
n=0
while running "$new" || running "$old"; do
	n=$((n + 1))
	[ "$n" -lt 50 ] || fail "still running after stop: new $new $(running "$new" && echo yes), old $old $(running "$old" && echo yes)"
	sleep 0.1
done
[ ! -e "$pidfile.draining" ] || fail "$pidfile.draining is left"
echo "pids $new and $old are gone"
echo "$me: ok"
