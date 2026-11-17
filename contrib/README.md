# contrib

Running durchlauferhitzer without systemd (FR-PROC-07). Not part of the
binary.

- `durchlauferhitzer-reload` — POSIX `sh` (dash, busybox ash, bash):
  restarts a running server without losing a connection. Steps and
  requirements:
  [`../README.md`](../README.md#restart-without-systemd). It sets
  `net.ipv4.tcp_migrate_req` to 1 and does not restore it. After `ready`
  it waits a grace (default 1 s, its sixth argument) before the SIGHUP,
  and keeps its lock until the old process has closed its listeners
  (at most 10 s, then a warning). The lock is the file
  `<pidfile>.lock/pid` (pid and start time), created with the shell's
  noclobber; a lock whose holder no longer runs is taken over. A new
  process it does not hand over gets `SIGHUP`; one that never reached
  its event loop is killed after 10 s. Its fork runs `setsid` and the
  binary only once it is listed in `<pidfile>.draining` (a "go" line
  on a private FIFO; a list it cannot write is exit 1): killed before
  that, the helper leaves nothing running.
- `openrc/durchlauferhitzer` — the OpenRC script (`start`, `stop`,
  `checkconfig`, `reload`), modelled on Alpine's haproxy script; `stop`
  also ends the processes a reload left (`<pidfile>.draining`: draining,
  or not handed over by a killed helper; each by pid and start time,
  checked again before every signal). Install it as
  `/etc/init.d/durchlauferhitzer`, the helper as
  `/usr/libexec/durchlauferhitzer/durchlauferhitzer-reload`. It does
  not restart the server after an abnormal exit, because
  `supervise-daemon` cannot carry a reload's hand-over
  ([`../README.md`](../README.md#openrc-no-restart-after-a-crash)).
  Use systemd where the server must come back on its own.
- `openrc/durchlauferhitzer.confd` — its settings, as
  `/etc/conf.d/durchlauferhitzer` (`ready_timeout`, `reload_grace`, ...).
- `test-openrc.sh <binary>` — POSIX `sh`, run as root on a throwaway
  system with OpenRC. CI runs it in the Alpine jobs, in a container. It
  installs into `/etc`, `/usr` and `/srv`: not for a real system. Its
  steps, in order:
  1. installs the script, its conf.d, the helper and the binary;
  2. marks OpenRC as booted;
  3. `start`, then a request;
  4. a slow client holds a connection;
  5. replaces the binary in place, points the config at a second root
     and runs `reload`: the answer comes from the new root, and the old
     process still drains;
  6. `status` reports started, and the new process holds only stdin,
     the log, the root, its listener and its anonymous inodes;
  7. a crash after the reload: `kill -9` of the new process makes
     `status` report `crashed` with exit status 32, and `restart` serves
     again with the draining old process ended;
  8. a crash without a reload: the same on the restarted process;
  9. `stop`: no server process and no draining list are left.

Tested by `driver/tests/restart.rs` (as root; CI's root step) and, under
OpenRC itself, by `test-openrc.sh`.
