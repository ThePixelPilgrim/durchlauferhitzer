//! The restart without systemd (FR-PROC-07): `contrib/durchlauferhitzer-reload`
//! run with `sh` (dash on Ubuntu) against real servers.
//!
//! - Under load (keep-alive and fresh clients, plus a batch of connections
//!   queued in the old process's backlog while it is stopped across the
//!   SIGHUP) with the binary replaced and a new route, every request gets
//!   a complete 200, the old process exits 0 and the pid file names the
//!   new one; two reloads back to back lose nothing either (Review Focus
//!   3), the first old process still draining a half-received request.
//! - A handover that cannot be lossless does not start (Review Focus 4):
//!   a bad config, a new process that fails at startup, a pid file that
//!   names another program; the old process serves on, untouched.
//!   A new process that dies during the grace gets the pid file pointed
//!   back at the old one (exit 1, no SIGHUP); one not ready in time is
//!   stopped and reaped; one whose helper is killed is listed in
//!   `<pidfile>.draining`, which OpenRC's `stop_post` ends, also while it
//!   is still the helper's fork on its way to the binary (a gated
//!   `setsid` stub); a helper killed before listing it (a gated `mv`
//!   stub) leaves nothing running.
//! - A `listen` address the new config drops is closed (documented); one
//!   it adds is served, with a new `user`, and nothing is lost.
//! - The lock: a second helper is refused while the holder runs; a stale
//!   one (holder gone, or its pid reused) is taken over, and of two
//!   helpers racing for it one at most proceeds. A new process that
//!   ignores SIGHUP (the helper under `nohup`) is left running, listed
//!   for `stop_post`.
//! - Steering: a ready new process gets every new connection before any
//!   SIGHUP; fresh clients across back-to-back reloads lose nothing (a
//!   kernel race resets a handshake completing on a listener as it
//!   closes); the grace holds the SIGHUP back; the helper keeps its lock
//!   until the old listeners closed.
//! - Control: with `tcp_migrate_req` 0 the connections queued in the old
//!   process at its SIGHUP are reset. The old process accepts at most 64
//!   connections per wakeup (`ACCEPT_BATCH`) before it reads the signal,
//!   so a batch of 400 leaves some queued when it closes the listener.
//!
//! These run as root (the helper writes `net.ipv4.tcp_migrate_req`, and
//! the server binds as root, then drops to `nobody`); otherwise they skip
//! with a printed reason, or fail with `DURCHLAUFERHITZER_REQUIRE_ROOT=1`.
//! A `tcp_migrate_req` that cannot be set to 1 skips them too, or fails
//! with `DURCHLAUFERHITZER_REQUIRE_MIGRATE=1`. The sysctl is global, so
//! the tests run one at a time.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    reason = "test helpers outside #[test] functions; clippy.toml allows these in tests"
)]

#[path = "support/child.rs"]
mod child;

#[path = "support/load.rs"]
mod load;

#[path = "support/site.rs"]
mod site;

/// The helper's refusals, grace, lock and `listen` changes.
#[path = "restart/helper.rs"]
mod helper;

use std::fs;
use std::io::{self, Write};
use std::net::TcpStream;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use child::{Containment, Running, exe, free_port, signal_pid, text};
use durchlauferhitzer::sys::user::lookup_user;
use load::{Load, Pattern, Tally, collect, connect, fresh_request, queue, read_response, warm_up};
use site::{
    HELPER, Migration, OPENRC, REQUIRE_ROOT, Site, alive, await_new_pid, bounded, can_restart,
    finish, hup_pending, loopback, reload_with, required, serial, set_migrate, start_time,
    wait_gone,
};

/// How long the old process may take to drain and exit.
const DRAIN_PATIENCE: Duration = Duration::from_secs(10);

/// How many connections are queued in the stopped old process.
const QUEUED: usize = 400;

/// The handover under load, with the old process stopped across the
/// helper's SIGHUP: connections queue in its backlog (and some of the
/// clients' too), then it continues. The helper waits for its listeners
/// to close, so it runs in the background until then.
fn stopped_handover(site: &Site, old: &Running, new_conf: &Path, migration: Migration) -> Tally {
    let addr = site.addr;
    signal_pid(old.pid(), "STOP");
    let queued = queue(addr, QUEUED);
    site.replace_binary();
    thread::scope(|scope| {
        let reload = scope.spawn(|| site.reload(new_conf));
        await_hup(old.pid(), &reload);
        // The helper set it to 1.
        assert!(set_migrate(migration));
        signal_pid(old.pid(), "CONT");
        site.assert_reload_ok(&reload.join().unwrap());
    });
    let tally = collect(queued, &["old", "new"]);
    assert!(set_migrate(Migration::On));
    tally
}

/// Waits until the stopped `pid` has the helper's SIGHUP pending; a
/// helper that ends without having sent it fails the test.
fn await_hup(pid: u32, reload: &thread::ScopedJoinHandle<'_, Output>) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !hup_pending(pid) {
        assert!(
            !reload.is_finished() || hup_pending(pid),
            "the helper ended, and pid {pid} has no SIGHUP pending"
        );
        assert!(Instant::now() < deadline, "no SIGHUP for pid {pid}");
        thread::sleep(Duration::from_millis(10));
    }
}

/// FR-PROC-07: no connection is lost, accepted or queued, and the binary
/// may change.
#[test]
fn reload_under_load_loses_nothing() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    let old_pid = old.pid();
    let load = Load::start(addr, &["old", "new"]);
    warm_up();
    let queued = stopped_handover(&site, &old, &new_conf, Migration::On);
    let status = old.wait_exit(DRAIN_PATIENCE);
    thread::sleep(Duration::from_millis(200));
    let load = load.finish();
    eprintln!("queued: {:?}; load: {:?}", queued.answers, load.answers);
    assert_eq!(status.code(), Some(0), "old: {status}\n{}", site.log_text());
    assert!(queued.failures.is_empty(), "queued: {queued:?}");
    assert_eq!(queued.answers.values().sum::<usize>(), QUEUED);
    // More than one accept batch was queued: some moved to the new process.
    assert!(
        queued.answers.get("new").is_some_and(|&n| n > 0),
        "{queued:?}"
    );
    assert!(load.failures.is_empty(), "load: {:?}", load.failures);
    assert!(load.answers.get("old").is_some_and(|&n| n > 0), "{load:?}");
    assert!(load.answers.get("new").is_some_and(|&n| n > 0), "{load:?}");
    let new_pid = site.pid();
    assert_ne!(new_pid, old_pid);
    assert_eq!(site.processes(), vec![new_pid]);
    for _ in 0..10 {
        assert_eq!(fresh_request(addr, &["new"]), Ok("new".to_owned()));
    }
}

/// FR-PROC-07 "Steering": once the new process is ready, before any
/// SIGHUP, every new connection reaches it, none the old process.
#[test]
fn ready_steers_new_connections_to_the_new_process() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let _old = site.start(&old_conf);
    let _new = site.start_ready(&new_conf);
    for _ in 0..50 {
        assert_eq!(fresh_request(addr, &["old", "new"]), Ok("new".to_owned()));
    }
}

/// How many back-to-back reloads the storm sees.
const STORM_RELOADS: usize = 6;

/// FR-PROC-07 "Steering": a probabilistic regression smoke test, not a
/// proof. Many fresh connections across several reloads lose nothing; a
/// connect reset at the old listener's close is the kernel race steering
/// avoids. Before steering it failed about 1 run in 10 here. The
/// deterministic guard is `ready_steers_new_connections_to_the_new_process`;
/// the grace's timing is pinned by
/// `the_old_process_gets_no_sighup_before_the_grace_ends`.
#[test]
fn reload_under_fresh_storm_loses_nothing() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let confs = [
        site.config("old", &[addr], "old"),
        site.config("new", &[addr], "new"),
    ];
    let _first = site.start(&confs[0]);
    let load = Load::with(addr, &["old", "new"], &[Pattern::Fresh; 12]);
    warm_up();
    for i in 0..STORM_RELOADS {
        site.assert_reload_ok(&site.reload(&confs[(i + 1) % 2]));
    }
    let deadline = Instant::now() + DRAIN_PATIENCE;
    while site.processes() != vec![site.pid()] {
        assert!(Instant::now() < deadline, "{:?}", site.processes());
        thread::sleep(Duration::from_millis(10));
    }
    let load = load.finish();
    eprintln!("load: {:?}", load.answers);
    assert!(load.failures.is_empty(), "load: {:?}", load.failures);
}

/// The helper releases its lock only once the old process has closed its
/// listeners: here the old process is stopped across its SIGHUP, and the
/// helper runs on until it continues.
#[test]
fn the_helper_waits_for_the_old_listeners_to_close() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    // The helper reads it to see the listeners: root without
    // CAP_SYS_PTRACE cannot (the server runs as `nobody`), and the helper
    // then warns and does not wait (README). A skip, or a failure where
    // root is required.
    if let Err(err) = fs::read_dir(format!("/proc/{}/fd", old.pid())) {
        assert!(
            !required(REQUIRE_ROOT),
            "/proc/<server>/fd cannot be read here ({err}), but {REQUIRE_ROOT}=1 requires it"
        );
        eprintln!("skipped: /proc/<server>/fd cannot be read here ({err})");
        return;
    }
    let lock = site.dir.path().join("durchlauferhitzer.pid.lock");
    signal_pid(old.pid(), "STOP");
    thread::scope(|scope| {
        let reload = scope.spawn(|| site.reload(&new_conf));
        await_hup(old.pid(), &reload);
        thread::sleep(Duration::from_millis(500));
        assert!(!reload.is_finished(), "the helper did not wait");
        assert!(lock.is_dir(), "the lock went early");
        signal_pid(old.pid(), "CONT");
        let output = reload.join().unwrap();
        site.assert_reload_ok(&output);
        assert_eq!(text(&output.stderr), "");
    });
    assert!(!lock.exists(), "the lock was left behind");
    assert_eq!(old.wait_exit(DRAIN_PATIENCE).code(), Some(0));
    assert_eq!(fresh_request(addr, &["new"]), Ok("new".to_owned()));
}

/// Control: without `tcp_migrate_req` the connections still queued in
/// the old process when it closes its listener are reset.
#[test]
fn without_migration_queued_connections_are_reset() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let old_conf = site.config("old", &[addr], "old");
    let new_conf = site.config("new", &[addr], "new");
    let mut old = site.start(&old_conf);
    let queued = stopped_handover(&site, &old, &new_conf, Migration::Off);
    let status = old.wait_exit(DRAIN_PATIENCE);
    assert_eq!(status.code(), Some(0));
    let reset = queued
        .failures
        .iter()
        .filter(|failure| failure.contains("reset"))
        .count();
    eprintln!("queued: {:?}; {reset} reset", queued.answers);
    assert!(reset > 0, "{queued:?}");
    assert_eq!(reset, queued.failures.len(), "{queued:?}");
}

/// Review Focus 3: the second handover runs while the first old process
/// still drains (a half-received request holds it); nothing is lost and
/// the pid file names the newest process.
#[test]
fn two_reloads_back_to_back_lose_nothing() {
    let _serial = serial();
    if !can_restart() {
        return;
    }
    let site = Site::new();
    let addr = site.addr;
    let first_conf = site.config("first", &[addr], "old");
    let second_conf = site.config("second", &[addr], "second");
    let third_conf = site.config("third", &[addr], "third");
    let mut first = site.start(&first_conf);
    let mut half = connect(addr).unwrap();
    half.write_all(b"GET /a.txt HTTP/1.1\r\nHost: exam")
        .unwrap();
    let load = Load::start(addr, &["old", "second", "third"]);
    warm_up();
    site.assert_reload_ok(&site.reload(&second_conf));
    let second = site.pid();
    site.assert_reload_ok(&site.reload(&third_conf));
    let third = site.pid();
    // Both earlier processes still ran at the second handover.
    assert_eq!(site.draining(), vec![first.pid(), second]);
    assert!(
        first.child.try_wait().unwrap().is_none(),
        "the first old process ended early"
    );
    wait_gone(second, DRAIN_PATIENCE);
    half.write_all(b"ple.test\r\n\r\n").unwrap();
    let response = read_response(&mut half).unwrap();
    assert_eq!(response.status, "HTTP/1.1 200 OK");
    assert_eq!(response.body, "old\n");
    assert!(response.close, "{response:?}");
    let status = first.wait_exit(DRAIN_PATIENCE);
    assert_eq!(status.code(), Some(0));
    let load = load.finish();
    assert!(load.failures.is_empty(), "load: {:?}", load.failures);
    assert_ne!(second, third);
    assert_eq!(site.processes(), vec![third]);
    assert_eq!(fresh_request(addr, &["third"]), Ok("third".to_owned()));
}
