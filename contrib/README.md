# contrib

Running durchlauferhitzer without systemd (FR-PROC-07). Not part of the
binary.

- `durchlauferhitzer-reload` — POSIX `sh` (dash, busybox ash): restarts a
  running server without losing a connection. Steps and requirements:
  [`../README.md`](../README.md#restart-without-systemd). It sets
  `net.ipv4.tcp_migrate_req` to 1 and does not restore it. After `ready`
  it waits a grace (default 1 s, its sixth argument) before the SIGHUP,
  and keeps its lock until the old process has closed its listeners
  (at most 10 s, then a warning). The lock is the file
  `<pidfile>.lock/pid` (pid and start time), created with the shell's
  noclobber; a lock whose holder no longer runs is taken over. A new
  process it does not hand over gets `SIGHUP`; one that never reached
  its event loop is killed after 10 s.
- `openrc/durchlauferhitzer` — the OpenRC script (`start`, `stop`,
  `checkconfig`, `reload`), modelled on Alpine's haproxy script; `stop`
  also ends the processes a reload left draining (`<pidfile>.draining`). Install
  it as `/etc/init.d/durchlauferhitzer`, the helper as
  `/usr/libexec/durchlauferhitzer/durchlauferhitzer-reload`.
- `openrc/durchlauferhitzer.confd` — its settings, as
  `/etc/conf.d/durchlauferhitzer` (`ready_timeout`, `reload_grace`, ...).
- `test-openrc.sh <binary>` — POSIX `sh`, as root on a throwaway system
  with OpenRC (CI: the Alpine jobs, in a container): installs the script,
  its conf.d, the helper and the binary, then `start`, a request,
  replaces the binary in place and points the config at a second root,
  `reload` (the answer comes from the new root) while a slow client keeps
  the old process draining, `status` (started), the new process's
  descriptors (only stdin, the log, the root, its listener and its
  anonymous inodes), `stop` (the new and the draining old process end).
  It installs into `/etc`, `/usr` and `/srv`: not for a real system.

Tested by `driver/tests/restart.rs` (as root; CI's root step) and, under
OpenRC itself, by `test-openrc.sh`.
