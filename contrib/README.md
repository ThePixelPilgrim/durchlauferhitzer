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
  `checkconfig`, `reload`), modelled on Alpine's haproxy script. Install
  it as `/etc/init.d/durchlauferhitzer`, the helper as
  `/usr/libexec/durchlauferhitzer/durchlauferhitzer-reload`.
  - `stop` also ends the processes a reload left in
    `<pidfile>.draining`: draining, or not handed over by a killed
    helper. Each is matched by pid and start time, checked again before
    every signal.
  - The pid file's process falls into one of three cases, as the
    README's table lists. A stranger's executable was read and is not
    the server's binary. An
    unknown one's executable cannot be read, for example without
    `CAP_SYS_PTRACE`.
  - `status` of a started service reports `crashed` with exit status 32
    for a stranger. For the server and for unknown it is OpenRC's own.
  - `stop` signals nothing for a stranger: it warns and removes the pid
    file. For the server and for unknown it is OpenRC's own.
  - It does not restart the server after an abnormal exit, because
    `supervise-daemon` cannot carry a reload's hand-over
    ([`../README.md`](../README.md#openrc-no-restart-after-a-crash)).
    Use systemd where the server must come back on its own.
- `openrc/durchlauferhitzer.confd` — its settings, as
  `/etc/conf.d/durchlauferhitzer` (`ready_timeout`, `reload_grace`, ...).
- `test-openrc.sh <binary>` — POSIX `sh`, run as root on a throwaway
  system with OpenRC. CI runs it in the Alpine jobs, in a container. It
  writes into the system's own directories, so it is not for a real
  system. Its steps, in order:
  1. It installs the service files and the binary.
  2. It marks OpenRC as booted.
  3. `start`, then a request.
  4. A slow client holds a connection.
  5. A `reload` onto a replaced binary and a second root. The answer
     then comes from the new root. The old process still drains.
  6. `status` reports started. The new process holds no descriptor
     beyond the ones it needs.
  7. A crash after the reload: `kill -9` of the new process. `status`
     then reports `crashed` with exit status 32. `restart` serves again,
     and the draining old process is ended.
  8. A crash without a reload: `kill -9` of the restarted process.
     `status` then reports `crashed` with exit status 32. `restart`
     serves again.
  9. A crash whose pid is reused: `kill -9` of the server, then the
     pid file names an unrelated `sleep`. `status` reports `crashed`
     with exit status 32. `restart` serves again, and the `sleep` still
     runs.
  10. `stop`: no server process and no draining list are left.

Tested by `driver/tests/restart.rs` (as root; CI's root step) and, under
OpenRC itself, by `test-openrc.sh`. The script's pid-file guard is also
tested without OpenRC, on every CI machine, by
[`../scripts/test-openrc-guard.sh`](../scripts/test-openrc-guard.sh).
