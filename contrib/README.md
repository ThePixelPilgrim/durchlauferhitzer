# contrib

Running durchlauferhitzer without systemd (FR-PROC-07). Not part of the
binary.

- `durchlauferhitzer-reload` — POSIX `sh` (dash, busybox ash): restarts a
  running server without losing a connection. Steps and requirements:
  [`../README.md`](../README.md#restart-without-systemd). It sets
  `net.ipv4.tcp_migrate_req` to 1 and does not restore it.
- `openrc/durchlauferhitzer` — the OpenRC script (`start`, `stop`,
  `checkconfig`, `reload`), modelled on Alpine's haproxy script. Install
  it as `/etc/init.d/durchlauferhitzer`, the helper as
  `/usr/libexec/durchlauferhitzer/durchlauferhitzer-reload`.
- `openrc/durchlauferhitzer.confd` — its settings, as
  `/etc/conf.d/durchlauferhitzer`.

Tested by `driver/tests/restart.rs` (as root; CI's root step).
